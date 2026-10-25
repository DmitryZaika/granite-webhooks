//! The daily onboarding run behind `POST /v1/jobs/onboarding-commitments`.
//!
//! Each tick: today's slot check (once per UTC day, shared by every
//! company), then up to 25 companies whose own run is due (every 23 hours,
//! `job_runs` key `onboarding-commitment:<id>`). Per company: the booking as
//! Calendly has it (or one made outside the widget), `commitment::daily`,
//! then the work it says is due: reminders, the fee notice, the fee invoice.
//! No Calendly, Stripe or mail work starts after [`RUN_BUDGET`]; a company
//! cut short is left for the next tick. Reports carry ids and codes only.

use super::calendly::{self, Calendly, CalendlyError, SlotEventTypes};
use super::commitment::{
    self, Availability, Booking, BookingCheck, Commitment, DailyFacts, Due, EventKind, NOTICE_DAYS,
    Outcome, ReminderStep, SLOT_WINDOW_BUSINESS_DAYS, Status,
};
use super::emails::{self, NoticeInput, ReminderInput};
use super::fee::{self, FeeError};
use super::store::{self, EMAIL_CYCLE_KIND, NOTICE_STEP, NewAvailabilityDay, SaveError, Stored};
use crate::billing::account::default_card;
use crate::billing::catalog::SIGNUP_TZ;
use crate::billing::reminder_emails::RenderedEmail;
use crate::billing::reminders::{ReminderSendResult, classify_send_failure, describe_card};
use crate::billing::store::{BillingStore, MySqlBillingStore};
use crate::job_runs;
use crate::state::AppState;
use crate::webhooks_client::OutgoingEmail;
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Weekday};
use lambda_http::tracing;
use serde::Serialize;
use std::sync::LazyLock;
use std::time::Instant;
use utoipa::ToSchema;

static POSTHOG_HTTP: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);

/// Companies taken per run.
pub const COMPANIES_PER_RUN: u32 = 25;
/// Each company's run is due this long after its last start.
pub const COMPANY_EVERY_HOURS: i64 = 23;
/// No Calendly, Stripe or mail work is started after this.
pub const RUN_BUDGET: std::time::Duration = std::time::Duration::from_secs(20);
/// Calendly requests one company's run may make.
pub const CALENDLY_CALLS_PER_COMPANY: u32 = 10;
/// Calendly takes at most 7 days of slots per request.
const MAX_SLOT_RANGE_DAYS: i64 = 7;
const SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// What one run did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, ToSchema)]
pub struct OnboardingRun {
    /// Companies whose daily run this run took.
    pub companies: usize,
    /// Reminder emails delivered to the mail service.
    pub reminders: usize,
    /// Fee notices recorded as sent (one per company).
    pub notices: usize,
    /// Fee invoices created or finished in Stripe.
    pub invoices: usize,
    /// Deadlines moved by a day without a bookable slot.
    pub tolled: usize,
    /// Commitments completed by their call (48 hours after it, no no-show).
    pub completed: usize,
    /// Failures (Calendly, Stripe, mail, database); each was reported and is
    /// retried by a later run.
    pub errors: usize,
}

#[derive(Debug, thiserror::Error)]
enum CompanyError {
    #[error("the commitment changed meanwhile")]
    Conflict,
    #[error("the run's time is up")]
    OutOfTime,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Fee(#[from] FeeError),
}

impl From<SaveError> for CompanyError {
    fn from(err: SaveError) -> Self {
        match err {
            SaveError::Conflict => Self::Conflict,
            SaveError::Database(err) => Self::Database(err),
        }
    }
}

impl CompanyError {
    fn code(&self) -> String {
        match self {
            Self::Conflict => "conflict".into(),
            Self::OutOfTime => "out_of_time".into(),
            Self::Database(_) => "database".into(),
            Self::Fee(err) => err.code(),
        }
    }

    /// Worth retrying on the next tick: a conflict, the run's time or the
    /// database. Anything else (Stripe, no customer) waits for the next day.
    const fn retry_soon(&self) -> bool {
        matches!(
            self,
            Self::Conflict
                | Self::OutOfTime
                | Self::Database(_)
                | Self::Fee(FeeError::Save(_) | FeeError::Database(_))
        )
    }
}

/// What one company's run may still spend: the run's time and its Calendly
/// requests.
#[derive(Debug)]
struct Budget {
    deadline: Instant,
    calendly_calls: u32,
}

impl Budget {
    const fn new(deadline: Instant) -> Self {
        Self {
            deadline,
            calendly_calls: CALENDLY_CALLS_PER_COMPANY,
        }
    }

    fn out_of_time(&self) -> bool {
        Instant::now() >= self.deadline
    }

    /// Before Stripe or mail work: `OutOfTime` once the run's time is up.
    fn check(&self) -> Result<(), CompanyError> {
        if self.out_of_time() {
            Err(CompanyError::OutOfTime)
        } else {
            Ok(())
        }
    }

    /// Takes one Calendly request: `Budget` once the time or the calls are
    /// used up.
    fn calendly(&mut self) -> Result<(), CalendlyError> {
        if self.calendly_calls == 0 || self.out_of_time() {
            return Err(CalendlyError::Budget);
        }
        self.calendly_calls -= 1;
        Ok(())
    }
}

/// An `$exception` for Error Tracking: ids and codes only.
async fn report(problem: &str, company_id: Option<i32>, error_code: &str) {
    tracing::error!(
        problem,
        ?company_id,
        error_code,
        "onboarding commitment problem"
    );
    let properties = serde_json::json!({
        "$exception_list": [{
            "type": "OnboardingCommitmentError",
            "value": problem,
            "mechanism": { "handled": true, "synthetic": false },
        }],
        "$process_person_profile": false,
        "error_source": "api",
        "job": "onboarding_commitments",
        "company_id": company_id,
        "error_code": error_code,
    });
    crate::posthog::capture(&POSTHOG_HTTP, "$exception", "granite-api", properties).await;
}

fn availability_of(result: &str) -> Availability {
    match result {
        "available" => Availability::Available,
        "no_slots" => Availability::NoSlots,
        "not_configured" => Availability::NotConfigured,
        "error" => Availability::Error,
        _ => Availability::NotChecked,
    }
}

/// The UTC time of a local ([`SIGNUP_TZ`]) midnight.
fn local_midnight(day: NaiveDate) -> NaiveDateTime {
    let midnight = day.and_time(NaiveTime::MIN);
    SIGNUP_TZ
        .from_local_datetime(&midnight)
        .earliest()
        .or_else(|| {
            SIGNUP_TZ
                .from_local_datetime(&(midnight + Duration::hours(1)))
                .earliest()
        })
        .map_or(midnight, |at| at.naive_utc())
}

/// The tolling window of `now`: from a minute ahead to the end of the fifth
/// business day (Mon to Fri) after today, both days local to [`SIGNUP_TZ`].
fn slot_window(now: NaiveDateTime) -> (NaiveDateTime, NaiveDateTime) {
    let mut day = SIGNUP_TZ.from_utc_datetime(&now).date_naive();
    let mut business_days = 0;
    while business_days < SLOT_WINDOW_BUSINESS_DAYS {
        day += Duration::days(1);
        if !matches!(day.weekday(), Weekday::Sat | Weekday::Sun) {
            business_days += 1;
        }
    }
    (
        now + Duration::minutes(1),
        local_midnight(day + Duration::days(1)),
    )
}

/// `[start, end)` cut into Calendly's ranges of at most 7 days.
fn slot_ranges(start: NaiveDateTime, end: NaiveDateTime) -> Vec<(NaiveDateTime, NaiveDateTime)> {
    let mut ranges = Vec::new();
    let mut from = start;
    while from < end {
        let to = (from + Duration::days(MAX_SLOT_RANGE_DAYS)).min(end);
        ranges.push((from, to));
        from = to;
    }
    ranges
}

/// Asks Calendly for bookable slots in the tolling window of `now`.
async fn check_slots(calendly: &Calendly, now: NaiveDateTime) -> NewAvailabilityDay {
    let (window_start_at, window_end_at) = slot_window(now);
    let mut day = NewAvailabilityDay {
        checked_on: now.date(),
        checked_at: now,
        result: "error",
        slots: 0,
        first_slot_at: None,
        window_start_at,
        window_end_at,
        event_type_uris: Vec::new(),
        error_code: None,
    };
    let uris = match calendly.slot_event_types().await {
        Ok(SlotEventTypes::Types(uris)) => uris,
        Ok(SlotEventTypes::NotConfigured) => {
            day.result = "not_configured";
            return day;
        }
        Err(err) => {
            day.error_code = Some(err.code());
            return day;
        }
    };
    for uri in &uris {
        for (start, end) in slot_ranges(window_start_at, window_end_at) {
            match calendly.available_slots(uri, start, end).await {
                Ok(slots) => {
                    day.slots = day
                        .slots
                        .saturating_add(i32::try_from(slots.len()).unwrap_or(i32::MAX));
                    day.first_slot_at = slots.into_iter().chain(day.first_slot_at).min();
                }
                Err(err) => {
                    day.error_code = Some(err.code());
                    day.event_type_uris = uris;
                    return day;
                }
            }
        }
    }
    day.result = if day.slots > 0 {
        "available"
    } else {
        "no_slots"
    };
    day.event_type_uris = uris;
    day
}

/// Today's slot check: read, or made once and recorded (the first run of
/// the day reports a missing setup or a Calendly failure).
async fn today_availability(state: &AppState, now: NaiveDateTime) -> sqlx::Result<Availability> {
    let pool = &state.pool;
    let today = now.date();
    if let Some(day) = store::availability_day(pool, today).await? {
        return Ok(availability_of(&day.result));
    }
    let day = check_slots(&state.onboarding_call.calendly, now).await;
    if store::insert_availability_day(pool, &day).await? {
        match day.result {
            "not_configured" => {
                tracing::warn!("onboarding slot check: no Calendly booking page is configured");
                report(
                    "Onboarding Calendly is not configured",
                    None,
                    "not_configured",
                )
                .await;
            }
            "error" => {
                let code = day.error_code.as_deref().unwrap_or("error");
                report("Onboarding slot check failed", None, code).await;
            }
            _ => {}
        }
    }
    Ok(store::availability_day(pool, today)
        .await?
        .map_or(Availability::NotChecked, |day| availability_of(&day.result)))
}

/// The stored booking as Calendly has it today.
async fn refresh_booking(
    calendly: &Calendly,
    stored: &Booking,
    now: NaiveDateTime,
    budget: &mut Budget,
) -> Result<BookingCheck, CalendlyError> {
    budget.calendly()?;
    let invitee = calendly.invitee(&stored.invitee_uri).await?;
    budget.calendly()?;
    let event = calendly.scheduled_event(&invitee.event_uri).await?;
    if invitee.active {
        if event.active {
            return Ok(BookingCheck::Active {
                no_show_at: invitee.no_show_at,
            });
        }
        return Ok(BookingCheck::Cancelled {
            by_host: event.cancellation.is_some_and(|c| c.by_host),
            at: event.cancellation.and_then(|c| c.at).unwrap_or(now),
        });
    }
    if invitee.rescheduled
        && let Some(new_uri) = invitee.new_invitee.as_deref()
    {
        budget.calendly()?;
        let new = calendly.invitee(new_uri).await?;
        budget.calendly()?;
        let new_event = calendly.scheduled_event(&new.event_uri).await?;
        let booking = calendly::booking(&new, &new_event, now);
        return Ok(if invitee.cancellation.is_some_and(|c| c.by_host) {
            BookingCheck::MovedByHost(booking)
        } else {
            BookingCheck::Moved(booking)
        });
    }
    Ok(BookingCheck::Cancelled {
        by_host: invitee.cancellation.is_some_and(|c| c.by_host)
            || event.cancellation.is_some_and(|c| c.by_host),
        at: invitee
            .cancellation
            .and_then(|c| c.at)
            .or_else(|| event.cancellation.and_then(|c| c.at))
            .unwrap_or(now),
    })
}

/// Where the search for an outside booking starts: signup, or just after
/// the start of the call marked as missed.
fn search_from(commitment: &Commitment) -> NaiveDateTime {
    commitment
        .no_show_call_start_at
        .map_or(commitment.created_at, |missed| {
            missed.max(commitment.created_at) + Duration::seconds(1)
        })
}

/// A call starting at `start` is not the missed one (nor older).
fn after_missed_call(commitment: &Commitment, start: NaiveDateTime) -> bool {
    commitment
        .no_show_call_start_at
        .is_none_or(|missed| start > missed)
}

/// A booking carrying the company's tag made outside the widget (a
/// reminder's link, a shared page), by one of its Managers or Admins, that
/// starts after a missed call.
async fn find_booking(
    state: &AppState,
    commitment: &Commitment,
    now: NaiveDateTime,
    budget: &mut Budget,
) -> Result<BookingCheck, CalendlyError> {
    let calendly = &state.onboarding_call.calendly;
    let company_id = commitment.company_id;
    let Ok(emails) = store::booker_emails(&state.pool, company_id).await else {
        return Ok(BookingCheck::Unknown);
    };
    if emails.is_empty() {
        return Ok(BookingCheck::None);
    }
    if calendly.cached_account().is_none() {
        budget.calendly()?;
    }
    let account = calendly.account().await?;
    let from = search_from(commitment);
    for email in &emails {
        budget.calendly()?;
        let events = calendly
            .active_events_with_invitee(&account.uri, email, from)
            .await?;
        for event in events
            .iter()
            .filter(|event| event.active && after_missed_call(commitment, event.start_at))
        {
            budget.calendly()?;
            let invitees = calendly.event_invitees(&event.uri, email).await?;
            if let Some(invitee) = invitees.iter().find(|invitee| {
                invitee.active && invitee.no_show_at.is_none() && invitee.carries_tag(company_id)
            }) {
                return Ok(BookingCheck::Found(calendly::booking(invitee, event, now)));
            }
        }
    }
    Ok(BookingCheck::None)
}

/// The booking check; `Unknown` when Calendly fails or the budget runs out.
async fn booking_check(
    state: &AppState,
    commitment: &Commitment,
    now: NaiveDateTime,
    budget: &mut Budget,
) -> BookingCheck {
    let calendly = &state.onboarding_call.calendly;
    if !calendly.is_enabled() {
        return if commitment.booking.is_some() {
            BookingCheck::Unknown
        } else {
            BookingCheck::None
        };
    }
    let checked = match &commitment.booking {
        Some(booking) => refresh_booking(calendly, booking, now, budget).await,
        None => find_booking(state, commitment, now, budget).await,
    };
    checked.unwrap_or_else(|err| {
        tracing::warn!(
            company_id = commitment.company_id,
            code = %err.code(),
            "onboarding booking not checked"
        );
        BookingCheck::Unknown
    })
}

/// Days a company's tolling was never decided for are looked back at most this far.
const MAX_EARLIER_DAYS: i64 = 60;

/// The days after the company's last tolling decision (or its signup) and
/// before today, each with the check recorded that day (`NotChecked` when
/// the job did not run).
async fn earlier_days(
    pool: &sqlx::MySqlPool,
    commitment: &Commitment,
    now: NaiveDateTime,
) -> sqlx::Result<Vec<(NaiveDate, Availability)>> {
    let today = now.date();
    let first = commitment
        .availability_checked_on
        .map_or(commitment.created_at.date(), |day| day + Duration::days(1))
        .max(today - Duration::days(MAX_EARLIER_DAYS));
    let Some(last) = today.pred_opt().filter(|last| *last >= first) else {
        return Ok(Vec::new());
    };
    let recorded = store::availability_days_between(pool, first, last).await?;
    Ok(first
        .iter_days()
        .take_while(|day| *day <= last)
        .map(|day| {
            let availability = recorded
                .iter()
                .find(|row| row.checked_on == day)
                .map_or(Availability::NotChecked, |row| availability_of(&row.result));
            (day, availability)
        })
        .collect())
}

async fn gather_facts(
    state: &AppState,
    commitment: &Commitment,
    availability: Availability,
    now: NaiveDateTime,
    budget: &mut Budget,
) -> sqlx::Result<DailyFacts> {
    let pool = &state.pool;
    let company_id = commitment.company_id;
    Ok(DailyFacts {
        booking: booking_check(state, commitment, now, budget).await,
        availability,
        earlier_days: earlier_days(pool, commitment, now).await?,
        cancellation_requested_at: store::cancellation_requested_at(pool, company_id).await?,
        subscription_ended: store::subscription_ended(pool, company_id).await?,
        charging_enabled: state.onboarding_call.fee_charging,
        consent_on_file: store::consent_on_file(pool, company_id).await?,
    })
}

async fn send(state: &AppState, to: &str, email: &RenderedEmail) -> ReminderSendResult {
    let message = OutgoingEmail {
        from_name: Some("Granite Manager".into()),
        from_address: fee::from_address(),
        to: vec![to.to_string()],
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
    match state
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

/// Sends a claimed email and records how it went; true when it went out.
async fn send_claimed(
    state: &AppState,
    company_id: i32,
    claim_id: i64,
    to: &str,
    email: &RenderedEmail,
    now: NaiveDateTime,
) -> sqlx::Result<bool> {
    let pool = &state.pool;
    match send(state, to, email).await {
        ReminderSendResult::Sent { message_id } => {
            store::mark_email(
                pool,
                company_id,
                claim_id,
                "sent",
                None,
                message_id.as_deref(),
                Some(now),
            )
            .await?;
            Ok(true)
        }
        ReminderSendResult::Rejected { code } => {
            store::mark_email(
                pool,
                company_id,
                claim_id,
                "failed",
                Some(code.as_str()),
                None,
                None,
            )
            .await?;
            report("Onboarding email refused", Some(company_id), &code).await;
            Ok(false)
        }
        ReminderSendResult::Unknown { code } => {
            store::mark_email(
                pool,
                company_id,
                claim_id,
                "unknown",
                Some(code.as_str()),
                None,
                None,
            )
            .await?;
            report("Onboarding email outcome unknown", Some(company_id), &code).await;
            Ok(false)
        }
    }
}

/// The reminder of `step` to each Manager, once each.
async fn send_reminders(
    state: &AppState,
    commitment: &Commitment,
    step: ReminderStep,
    now: NaiveDateTime,
    budget: &Budget,
    run: &mut OnboardingRun,
) -> Result<(), CompanyError> {
    let pool = &state.pool;
    let company_id = commitment.company_id;
    let managers = store::managers(pool, company_id).await?;
    let company_name = store::company_name(pool, company_id).await?;
    for manager in managers {
        budget.check()?;
        let Some(claim_id) = store::claim_email(
            pool,
            company_id,
            manager.user_id,
            commitment.created_at,
            step.as_str(),
            now,
        )
        .await?
        else {
            continue;
        };
        let email = emails::render_reminder(&ReminderInput {
            step,
            booked: commitment.status == Status::Booked,
            call_start_at: commitment.booking.as_ref().map(|call| call.start_at),
            company_name: &company_name,
            user_name: manager.name.as_deref(),
            deadline_at: commitment.deadline_at,
            fee_cents: commitment.fee_cents,
            app_url: &state.app_url,
        });
        if send_claimed(state, company_id, claim_id, &manager.email, &email, now).await? {
            run.reminders += 1;
        }
    }
    Ok(())
}

/// When the first fee notice of the cycle went out, if one did.
async fn first_notice_at(
    pool: &sqlx::MySqlPool,
    company_id: i32,
    cycle_started_at: NaiveDateTime,
) -> sqlx::Result<Option<NaiveDateTime>> {
    sqlx::query_scalar(
        "SELECT MIN(sent_at) FROM billing_reminder_emails
          WHERE company_id = ? AND cycle_kind = ? AND cycle_started_at = ? AND step = ?
            AND status = 'sent'",
    )
    .bind(company_id)
    .bind(EMAIL_CYCLE_KIND)
    .bind(cycle_started_at)
    .bind(NOTICE_STEP)
    .fetch_one(pool)
    .await
}

/// A booked call ends at or before the charge: like the rules, it holds the
/// fee and completes the commitment.
fn booked_before_charge(booking: Option<&Booking>, charge_at: NaiveDateTime) -> bool {
    booking.is_some_and(|booking| booking.end_at <= charge_at)
}

/// The fee notice of the cycle starting at `cycle` to each Manager, once
/// each, all naming the charge date of its first email. When the first went
/// out (now or in an earlier run), if one did.
async fn send_notice(
    state: &AppState,
    commitment: &Commitment,
    cycle: NaiveDateTime,
    now: NaiveDateTime,
    budget: &Budget,
) -> Result<Option<NaiveDateTime>, CompanyError> {
    let pool = &state.pool;
    let company_id = commitment.company_id;
    let first = first_notice_at(pool, company_id, cycle).await?;
    let charge_at = first.unwrap_or(now) + Duration::days(NOTICE_DAYS);
    let managers = store::managers(pool, company_id).await?;
    let company_name = store::company_name(pool, company_id).await?;
    budget.check()?;
    let record = MySqlBillingStore(pool).by_company(company_id).await?;
    let method = default_card(&state.stripe, record.as_ref())
        .await
        .map(|card| describe_card(&card));
    let mut sent = false;
    for manager in managers {
        budget.check()?;
        let Some(claim_id) =
            store::claim_email(pool, company_id, manager.user_id, cycle, NOTICE_STEP, now).await?
        else {
            continue;
        };
        let email = emails::render_notice(&NoticeInput {
            company_name: &company_name,
            user_name: manager.name.as_deref(),
            fee_cents: commitment.fee_cents,
            deadline_at: commitment.deadline_at,
            charge_at,
            payment_method: method.as_deref(),
            booked_before_charge: booked_before_charge(commitment.booking.as_ref(), charge_at),
            app_url: &state.app_url,
        });
        sent |= send_claimed(state, company_id, claim_id, &manager.email, &email, now).await?;
    }
    Ok(first.or_else(|| sent.then_some(now)))
}

/// Saves the outcome when it changes anything; the commitment as stored now.
async fn save(
    state: &AppState,
    stored: Stored,
    outcome: Outcome,
    now: NaiveDateTime,
    run: &mut OnboardingRun,
) -> Result<Stored, CompanyError> {
    if !store::changes(&stored, &outcome) {
        return Ok(stored);
    }
    let version = store::save(&state.pool, stored.version, &outcome, now).await?;
    let count = |kind: EventKind| outcome.events.iter().filter(|e| e.kind == kind).count();
    run.tolled += count(EventKind::Tolled);
    run.completed += count(EventKind::Completed);
    Ok(Stored {
        commitment: outcome.commitment,
        version,
    })
}

async fn company(
    state: &AppState,
    company_id: i32,
    availability: Availability,
    now: NaiveDateTime,
    budget: &mut Budget,
    run: &mut OnboardingRun,
) -> Result<(), CompanyError> {
    let pool = &state.pool;
    store::sweep_stuck_emails(pool, company_id, now).await?;
    let Some(stored) = store::load(pool, company_id).await? else {
        return Ok(());
    };
    if stored.commitment.status == Status::FeeInvoiced {
        // An invoice a crash left unfinished (draft, or never attempted).
        budget.check()?;
        if matches!(
            fee::charge(state, &stored, now).await?,
            fee::Charged::Paid | fee::Charged::Failed
        ) {
            run.invoices += 1;
        }
        return Ok(());
    }
    let facts = gather_facts(state, &stored.commitment, availability, now, budget).await?;
    let daily = commitment::daily(&stored.commitment, &facts, now);
    let mut stored = save(state, stored, daily.outcome, now, run).await?;
    let mails = state.webhooks.is_enabled();
    for due in daily.due {
        match due {
            Due::Reminder(_) | Due::Notice if !mails => {
                tracing::warn!(
                    company_id,
                    "onboarding email skipped: mail is not configured"
                );
            }
            Due::Reminder(step) => {
                send_reminders(state, &stored.commitment, step, now, budget, run).await?;
            }
            Due::Notice => {
                let Some(cycle) = stored.commitment.notice_cycle_at else {
                    run.errors += 1;
                    report(
                        "Onboarding notice due without its cycle",
                        Some(company_id),
                        "no_notice_cycle",
                    )
                    .await;
                    continue;
                };
                if let Some(sent_at) =
                    send_notice(state, &stored.commitment, cycle, now, budget).await?
                {
                    let outcome = commitment::notice_sent(&stored.commitment, sent_at);
                    stored = save(state, stored, outcome, now, run).await?;
                    run.notices += 1;
                }
            }
            Due::Invoice => {
                budget.check()?;
                if !matches!(
                    fee::charge(state, &stored, now).await?,
                    fee::Charged::Withdrawn
                ) {
                    run.invoices += 1;
                }
            }
        }
    }
    Ok(())
}

/// One run at `now` (tests pass a fixed clock; the route passes
/// `billing::now()`).
pub async fn run_at(state: &AppState, now: NaiveDateTime) -> OnboardingRun {
    let deadline = Instant::now() + RUN_BUDGET;
    let pool = &state.pool;
    let mut run = OnboardingRun::default();
    let availability = match today_availability(state, now).await {
        Ok(availability) => availability,
        Err(err) => {
            run.errors += 1;
            tracing::warn!(error = %err, "onboarding slot check not recorded");
            report("Onboarding slot check not recorded", None, "database").await;
            Availability::NotChecked
        }
    };
    let cutoff = now - Duration::hours(COMPANY_EVERY_HOURS);
    let ids = match store::due_companies(pool, cutoff, COMPANIES_PER_RUN).await {
        Ok(ids) => ids,
        Err(_) => {
            run.errors += 1;
            report("Onboarding companies not listed", None, "database").await;
            return run;
        }
    };
    for company_id in ids {
        if Instant::now() >= deadline {
            break;
        }
        let key = format!("onboarding-commitment:{company_id}");
        let claim =
            match job_runs::claim(pool, &key, Duration::hours(COMPANY_EVERY_HOURS), now).await {
                Ok(Some(claim)) => claim,
                Ok(None) => continue,
                Err(_) => {
                    run.errors += 1;
                    report("Onboarding run not claimed", Some(company_id), "database").await;
                    continue;
                }
            };
        run.companies += 1;
        let mut budget = Budget::new(deadline);
        let result = company(state, company_id, availability, now, &mut budget, &mut run).await;
        let settled = match &result {
            Ok(()) => claim.finish(pool, now).await,
            Err(CompanyError::Conflict) => {
                tracing::warn!(
                    company_id,
                    "onboarding commitment changed meanwhile; retried"
                );
                claim.undo(pool).await
            }
            Err(CompanyError::OutOfTime) => {
                tracing::info!(
                    company_id,
                    "onboarding run out of time; left for the next tick"
                );
                claim.undo(pool).await
            }
            Err(err) => {
                run.errors += 1;
                report(
                    "Onboarding commitment run failed",
                    Some(company_id),
                    &err.code(),
                )
                .await;
                if err.retry_soon() {
                    claim.undo(pool).await
                } else {
                    claim.finish(pool, now).await
                }
            }
        };
        if let Err(err) = settled {
            tracing::warn!(error = %err, company_id, "onboarding run not recorded");
        }
    }
    run
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").unwrap()
    }

    #[test]
    fn a_friday_window_ends_after_next_friday_local_and_takes_two_requests() {
        // Friday 14:00 EDT.
        let now = at("2026-10-23 18:00");
        let (start, end) = slot_window(now);
        assert_eq!(start, at("2026-10-23 18:01"));
        assert_eq!(end, at("2026-10-31 04:00"));
        assert_eq!(
            slot_ranges(start, end),
            vec![
                (at("2026-10-23 18:01"), at("2026-10-30 18:01")),
                (at("2026-10-30 18:01"), at("2026-10-31 04:00")),
            ]
        );
        // Saturday 02:00 UTC is still Friday 22:00 EDT.
        let late = at("2026-10-24 02:00");
        let (start, end) = slot_window(late);
        assert_eq!(end, at("2026-10-31 04:00"));
        assert_eq!(slot_ranges(start, end).len(), 2);
    }

    #[test]
    fn a_saturday_window_ends_the_same_friday_and_takes_one_request() {
        // Saturday 12:00 EDT, and Saturday 23:00 EDT (Sunday in UTC).
        for now in [at("2026-10-24 16:00"), at("2026-10-25 03:00")] {
            let (start, end) = slot_window(now);
            assert_eq!(end, at("2026-10-31 04:00"));
            assert_eq!(slot_ranges(start, end), vec![(start, end)]);
        }
    }

    #[test]
    fn the_window_ends_at_local_midnight_across_the_dst_change() {
        // Friday 11:00 EDT; the window ends Saturday Nov 7, 00:00 EST.
        let (_, end) = slot_window(at("2026-10-30 15:00"));
        assert_eq!(end, at("2026-11-07 05:00"));
        assert!(slot_ranges(end, end).is_empty());
    }

    #[test]
    fn the_outside_booking_search_starts_after_a_missed_call() {
        let base =
            commitment::new_commitment(7, at("2026-10-01 12:00"), at("2026-10-31 12:00"), 60_000)
                .commitment;
        assert_eq!(search_from(&base), at("2026-10-01 12:00"));
        assert!(after_missed_call(&base, at("2026-10-02 09:00")));
        // Called at 15:00, marked missed at 16:30: a call at 15:30 is new.
        let missed = Commitment {
            no_show_at: Some(at("2026-10-10 16:30")),
            no_show_call_start_at: Some(at("2026-10-10 15:00")),
            ..base
        };
        assert_eq!(
            search_from(&missed),
            at("2026-10-10 15:00") + Duration::seconds(1)
        );
        assert!(!after_missed_call(&missed, at("2026-10-10 14:00")));
        assert!(!after_missed_call(&missed, at("2026-10-10 15:00")));
        assert!(after_missed_call(&missed, at("2026-10-10 15:30")));
        assert!(after_missed_call(&missed, at("2026-10-12 15:00")));
    }

    #[test]
    fn a_call_ending_by_the_charge_counts_as_booked_before_it() {
        let charge_at = at("2026-11-27 15:00");
        let call = |start: &str| Booking {
            event_uri: "https://api.calendly.com/scheduled_events/1".into(),
            invitee_uri: "https://api.calendly.com/scheduled_events/1/invitees/1".into(),
            start_at: at(start),
            end_at: at(start) + Duration::minutes(30),
            reschedule_url: None,
            cancel_url: None,
            booked_at: at("2026-11-01 12:00"),
        };
        assert!(!booked_before_charge(None, charge_at));
        assert!(booked_before_charge(
            Some(&call("2026-11-27 14:30")),
            charge_at
        ));
        assert!(!booked_before_charge(
            Some(&call("2026-11-27 14:31")),
            charge_at
        ));
        assert!(!booked_before_charge(
            Some(&call("2026-11-27 15:00")),
            charge_at
        ));
    }

    #[test]
    fn a_company_gets_ten_calendly_requests_and_none_past_the_deadline() {
        let mut budget = Budget::new(Instant::now() + std::time::Duration::from_secs(60));
        for _ in 0..CALENDLY_CALLS_PER_COMPANY {
            assert_eq!(budget.calendly(), Ok(()));
        }
        assert_eq!(budget.calendly(), Err(CalendlyError::Budget));
        assert!(budget.check().is_ok());

        let mut late = Budget::new(Instant::now());
        assert_eq!(late.calendly(), Err(CalendlyError::Budget));
        assert!(matches!(late.check(), Err(CompanyError::OutOfTime)));
        assert!(CompanyError::OutOfTime.retry_soon());
    }
}
