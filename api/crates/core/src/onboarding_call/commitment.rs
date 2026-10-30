//! The onboarding commitment's rules (Terms section 11), pure: every
//! transition takes the commitment, the facts and `now`, and returns the new
//! commitment plus the timeline events to store.
//!
//! Onboarding with Dima is free. When it is not completed by the deadline
//! (as moved), a notice goes to the Managers and the next subscription
//! charge at least [`NOTICE_DAYS`] after it also takes the following month
//! in advance ([`AdvanceTerms`]): a pending invoice item added within
//! [`ITEM_WINDOW_DAYS`] before that charge, credited back to the customer's
//! balance once its invoice is paid. Nothing is paid more overall, only
//! earlier.
//!
//! No I/O, no clock.

use crate::billing::catalog::{SIGNUP_TZ, deadline_on, signup_day};
use chrono::{Duration, Months, NaiveDate, NaiveDateTime, NaiveTime, TimeZone};
use serde_json::{Value, json};

pub const DEADLINE_DAYS: i64 = 30;
pub const EXTENSION_DAYS: i64 = 14;
/// A no-show recorded later than this after the call's end does not count.
pub const NO_SHOW_WINDOW_HOURS: i64 = 48;
/// The advance is charged no sooner than this after its notice email.
pub const NOTICE_DAYS: i64 = 7;
/// The advance's invoice item is added within this many days before its
/// charge (the daily run comes about every 23 hours).
pub const ITEM_WINDOW_DAYS: i64 = 3;
/// A pending item still on no invoice this long after its charge date: the
/// subscription's charge moved.
pub const CHARGE_LATE_HOURS: i64 = 24;
/// Reminder emails, in days after signup.
pub const REMINDER_DAYS: [i64; 3] = [7, 21, 27];
/// Tolling: a day tolls when no slot is bookable within this many business
/// days (Mon to Fri, local to `SIGNUP_TZ`) after it.
pub const SLOT_WINDOW_BUSINESS_DAYS: u32 = 5;

const SECS_PER_DAY: i64 = 86_400;
/// Subscription periods looked ahead for a charge date, at most.
const MAX_PERIODS: u32 = 36;

/// `company_onboarding_commitments.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Status {
    Pending,
    Booked,
    Completed,
    Waived,
    Suspended,
    NoticeSent,
    /// The advance's pending invoice item is in Stripe.
    AdvanceScheduled,
    /// The invoice with the advance was paid and the month credited back.
    AdvancePaid,
    /// The advance was due while charging was off or without consent:
    /// never charged.
    AdvanceSkipped,
    Void,
}

impl Status {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Booked => "booked",
            Self::Completed => "completed",
            Self::Waived => "waived",
            Self::Suspended => "suspended",
            Self::NoticeSent => "notice_sent",
            Self::AdvanceScheduled => "advance_scheduled",
            Self::AdvancePaid => "advance_paid",
            Self::AdvanceSkipped => "advance_skipped",
            Self::Void => "void",
        }
    }

    /// Also reads the onboarding fee's old values, which are never written:
    /// `fee_skipped` as skipped, the others as void.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "booked" => Some(Self::Booked),
            "completed" => Some(Self::Completed),
            "waived" => Some(Self::Waived),
            "suspended" => Some(Self::Suspended),
            "notice_sent" => Some(Self::NoticeSent),
            "advance_scheduled" => Some(Self::AdvanceScheduled),
            "advance_paid" => Some(Self::AdvancePaid),
            "advance_skipped" | "fee_skipped" => Some(Self::AdvanceSkipped),
            "void" | "fee_invoiced" | "fee_paid" | "fee_failed" => Some(Self::Void),
            _ => None,
        }
    }

    /// Nothing changes it any more: completed, waived, advance paid, advance
    /// skipped, void. (An invoice item recorded on it is still settled.)
    pub const fn is_final(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Waived | Self::AdvancePaid | Self::AdvanceSkipped | Self::Void
        )
    }
}

/// The booked call, as Calendly has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Booking {
    pub invitee_uri: String,
    pub event_uri: String,
    pub start_at: NaiveDateTime,
    pub end_at: NaiveDateTime,
    pub reschedule_url: Option<String>,
    pub cancel_url: Option<String>,
    /// When we recorded it (not Calendly's time).
    pub booked_at: NaiveDateTime,
}

/// What the notice states and the charge carries, fixed when the notice
/// cycle starts ([`advance_terms`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdvanceTerms {
    /// One month of the live subscription's items (unit amount x quantity:
    /// base, add-ons, extra users), before tax.
    pub advance_cents: i64,
    /// The subscription invoice the advance goes on: the first at least
    /// [`NOTICE_DAYS`] after the notice.
    pub charge_at: NaiveDateTime,
    /// The month the advance pays for: the period after `charge_at`
    /// (`prepaid_from` is the renewal its credit pays).
    pub prepaid_from: NaiveDateTime,
    pub prepaid_until: NaiveDateTime,
}

/// One row of `company_onboarding_commitments` (all times UTC).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commitment {
    pub company_id: i32,
    pub status: Status,
    /// Signup: reminders count from here.
    pub created_at: NaiveDateTime,
    pub deadline_at: NaiveDateTime,
    pub original_deadline_at: NaiveDateTime,
    pub tolled_days: i32,
    pub extension_used: bool,
    /// The advance the signup consent stated: one month at the plan chosen
    /// (base, add-ons, extra users of the user estimate), before tax.
    pub signup_advance_cents: i64,
    pub booking: Option<Booking>,
    pub completed_at: Option<NaiveDateTime>,
    pub completed_by: Option<i32>,
    pub no_show_at: Option<NaiveDateTime>,
    /// Start of the call the latest no-show was for: that call and older
    /// ones never count again.
    pub no_show_call_start_at: Option<NaiveDateTime>,
    pub suspended_at: Option<NaiveDateTime>,
    /// Kept while suspended: a withdrawal at least [`NOTICE_DAYS`] before the
    /// charge resumes the notice with its charge date.
    pub notice_sent_at: Option<NaiveDateTime>,
    /// When the current notice fell due (`daily` sets it): the notice
    /// emails are deduped per cycle, so a withdrawn and later resumed notice
    /// is sent again. Cleared by a suspension, its withdrawal or a moved
    /// deadline.
    pub notice_cycle_at: Option<NaiveDateTime>,
    /// Set when the notice cycle starts, kept with the notice.
    pub terms: Option<AdvanceTerms>,
    /// The advance's invoice item (`ii_...`) while it is in Stripe: pending,
    /// or on the invoice at `charge_at`. Kept after a transition that no
    /// longer wants it (`Due::SettleItem` deletes it then).
    pub advance_item_id: Option<String>,
    /// The subscription invoice that carried the item.
    pub stripe_invoice_id: Option<String>,
    /// The customer balance transaction that credited the month back.
    pub advance_credit_id: Option<String>,
    /// Staff hold the advance for review (Terms 11.7): no notice and no
    /// item while held.
    pub review_hold_at: Option<NaiveDateTime>,
    pub review_hold_by: Option<i32>,
    /// The last local day (`SIGNUP_TZ`) tolling was decided for.
    pub availability_checked_on: Option<NaiveDate>,
}

/// `company_onboarding_commitment_events.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Created,
    Booked,
    Rescheduled,
    BookingCancelled,
    NoShow,
    Tolled,
    Extended,
    Waived,
    Completed,
    Suspended,
    Resumed,
    NoticeSent,
    /// The notice was dropped: a new one comes before any charge.
    NoticeWithdrawn,
    AdvanceScheduled,
    /// The item left Stripe (deleted by us, or gone) before any charge.
    AdvanceWithdrawn,
    AdvanceInvoiced,
    AdvancePaid,
    AdvanceSkipped,
    Voided,
    ReviewHeld,
    ReviewReleased,
}

impl EventKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Booked => "booked",
            Self::Rescheduled => "rescheduled",
            Self::BookingCancelled => "booking_cancelled",
            Self::NoShow => "no_show",
            Self::Tolled => "tolled",
            Self::Extended => "extended",
            Self::Waived => "waived",
            Self::Completed => "completed",
            Self::Suspended => "suspended",
            Self::Resumed => "resumed",
            Self::NoticeSent => "notice_sent",
            Self::NoticeWithdrawn => "notice_withdrawn",
            Self::AdvanceScheduled => "advance_scheduled",
            Self::AdvanceWithdrawn => "advance_withdrawn",
            Self::AdvanceInvoiced => "advance_invoiced",
            Self::AdvancePaid => "advance_paid",
            Self::AdvanceSkipped => "advance_skipped",
            Self::Voided => "voided",
            Self::ReviewHeld => "review_held",
            Self::ReviewReleased => "review_released",
        }
    }
}

/// A timeline entry: `detail` holds the facts behind it (reason, old and
/// new deadline, call times, invoice and item ids).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub kind: EventKind,
    pub from: Option<Status>,
    pub to: Status,
    /// The deadline after the change.
    pub deadline_at: NaiveDateTime,
    /// The superuser who acted; `None` for the customer's booking or the job.
    pub actor: Option<i32>,
    pub detail: Value,
}

/// The new commitment and what to append to its timeline. Save it when it
/// differs from the old one (a field like `availability_checked_on` can
/// change without an event).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub commitment: Commitment,
    pub events: Vec<Event>,
}

/// Why an action is refused (the routes answer 409 with `message()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Completed, waived, paid, skipped or void.
    Final,
    /// The advance is on a subscription invoice: it is credited back once
    /// paid.
    AlreadyInvoiced,
    /// The one extension was used.
    AlreadyExtended,
    /// The extension would still leave the deadline in the past.
    ExtensionStillPast,
    /// No call is booked.
    NotBooked,
    /// A no-show before the booked call started.
    CallNotStarted,
    /// A no-show more than 48 hours after the call ended: the call counts
    /// as held (Terms 11.3).
    NoShowTooLate,
    /// The call starts at or before the start of the call a no-show was
    /// recorded for: it is the missed call (or an older one) and never
    /// counts again.
    MissedCall,
    /// Held for review already.
    AlreadyHeld,
    /// Not held for review.
    NotHeld,
}

impl Refusal {
    pub const fn message(self) -> &'static str {
        match self {
            Self::Final => "Onboarding is already settled for this company.",
            Self::AlreadyInvoiced => {
                "The advance payment is already on a subscription invoice. It is credited back on the next invoice once paid."
            }
            Self::AlreadyExtended => "The 14-day extension was already used.",
            Self::ExtensionStillPast => "Even with 14 more days the deadline would be in the past.",
            Self::NotBooked => "No onboarding call is booked.",
            Self::CallNotStarted => "The booked call has not started yet.",
            Self::NoShowTooLate => "The call ended more than 48 hours ago, so it counts as held.",
            Self::MissedCall => "This call was marked as missed. Please book a new time.",
            Self::AlreadyHeld => "The advance payment is already held for review.",
            Self::NotHeld => "The advance payment is not held for review.",
        }
    }
}

/// A superuser's action (`PUT /v1/superuser/companies/{id}/onboarding-commitment`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// The one 14-day extension the customer asked for.
    Extend,
    /// No advance: Granite's decision (a dispute, a goodwill gesture).
    Waive,
    /// Onboarding done (say, on a call outside Calendly).
    Complete,
    /// The customer missed the booked call.
    NoShow,
    /// Hold the advance for review (the customer replied to the notice).
    Hold,
    /// End the review hold.
    Release,
}

/// Every action, in the order the superuser page lists them.
pub const ACTIONS: [Action; 6] = [
    Action::Extend,
    Action::Waive,
    Action::Complete,
    Action::NoShow,
    Action::Hold,
    Action::Release,
];

/// The stored booking as Calendly has it today (the daily refresh).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BookingCheck {
    /// No booking stored and none found.
    None,
    /// No booking stored; a booking carrying our tag was found (made outside
    /// the widget, say from a reminder email's link).
    Found(Booking),
    /// The stored booking is still on; `no_show_at` when Calendly has a
    /// no-show mark (its creation time).
    Active { no_show_at: Option<NaiveDateTime> },
    /// The customer rescheduled the stored booking to this one.
    Moved(Booking),
    /// Granite (the host) rescheduled the stored booking to this one.
    MovedByHost(Booking),
    /// The stored booking was cancelled, by Granite when `by_host`.
    Cancelled { by_host: bool, at: NaiveDateTime },
    /// Calendly could not be asked: change nothing about the booking.
    Unknown,
}

/// Today's slot check (`onboarding_availability_days.result`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    /// No check for today yet: no tolling decision.
    NotChecked,
    Available,
    NoSlots,
    /// No booking page could be resolved.
    NotConfigured,
    /// Calendly failed: we cannot show a slot was offered, so it tolls.
    Error,
}

/// Everything the daily run learned about one company.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DailyFacts {
    pub booking: BookingCheck,
    pub availability: Availability,
    /// The local days after `availability_checked_on` (or signup) and before
    /// today, oldest first, with the check recorded for each (`NotChecked`
    /// when the job did not run that day). Any day but `Available` tolls: a
    /// day we cannot show a slot for counts against us.
    pub earlier_days: Vec<(NaiveDate, Availability)>,
    /// `requested_at` of the company's latest `billing_cancellation_requests`
    /// row that is not withdrawn (scheduled, or completed once the
    /// subscription ended), if any.
    pub cancellation_requested_at: Option<NaiveDateTime>,
    /// `requested_at` and `withdrawn_at` of the company's requests withdrawn
    /// after the stored notice was sent (none without a notice).
    pub withdrawn_cancellations: Vec<(NaiveDateTime, NaiveDateTime)>,
    /// The subscription is cancelled for real, or the company is deleted.
    pub subscription_ended: bool,
    /// The company has a running subscription (trialing, active or past
    /// due). Without one the deadline voids the commitment: nothing is ever
    /// noticed or charged.
    pub subscription_live: bool,
    /// `ONBOARDING_ADVANCE_CHARGING=1`.
    pub charging_enabled: bool,
    /// The company accepted the `onboarding_advance` consent.
    pub consent_on_file: bool,
}

/// `billing_reminder_emails.step` of an `onboarding` cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReminderStep {
    Day7,
    Day21,
    Day27,
}

const REMINDER_STEPS: [ReminderStep; 3] =
    [ReminderStep::Day7, ReminderStep::Day21, ReminderStep::Day27];

impl ReminderStep {
    /// `day_7`, `day_21`, `day_27`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Day7 => "day_7",
            Self::Day21 => "day_21",
            Self::Day27 => "day_27",
        }
    }

    pub const fn days(self) -> i64 {
        match self {
            Self::Day7 => REMINDER_DAYS[0],
            Self::Day21 => REMINDER_DAYS[1],
            Self::Day27 => REMINDER_DAYS[2],
        }
    }
}

/// Work the daily run must do after saving `outcome`, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Due {
    /// The recorded invoice item: delete it when no longer wanted, record
    /// its invoice, credit the month once that invoice is paid.
    SettleItem,
    /// Email this reminder (deduped per manager in `billing_reminder_emails`).
    Reminder(ReminderStep),
    /// Fix the terms from the live subscription ([`notice_terms`]), email the
    /// notice (deduped per `notice_cycle_at`, which `daily` has set), and on
    /// a sent email call [`notice_sent`].
    Notice,
    /// The item should be in Stripe now: find it, else add it
    /// ([`advance_scheduled`]); on or after the charge date with none, the
    /// notice is dropped ([`drop_notice`]).
    AddItem,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Daily {
    pub outcome: Outcome,
    pub due: Vec<Due>,
}

fn ts(at: NaiveDateTime) -> String {
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// `wall` in [`SIGNUP_TZ`] as UTC: the earlier time when the clock is set
/// back, an hour on when it is set forward past `wall`.
fn local_to_utc(wall: NaiveDateTime) -> NaiveDateTime {
    SIGNUP_TZ
        .from_local_datetime(&wall)
        .earliest()
        .or_else(|| {
            SIGNUP_TZ
                .from_local_datetime(&(wall + Duration::hours(1)))
                .earliest()
        })
        .as_ref()
        .map_or(wall, chrono::DateTime::naive_utc)
}

/// The day of `at` (UTC) in [`SIGNUP_TZ`].
pub fn local_day(at: NaiveDateTime) -> NaiveDate {
    SIGNUP_TZ.from_utc_datetime(&at).date_naive()
}

/// The start (00:00 local) of `day` in [`SIGNUP_TZ`], as UTC.
pub fn start_of_local_day(day: NaiveDate) -> NaiveDateTime {
    local_to_utc(day.and_time(NaiveTime::MIN))
}

/// The end of `day` in [`SIGNUP_TZ`] (23:59:59 local), as UTC.
pub fn end_of_local_day(day: NaiveDate) -> NaiveDateTime {
    local_to_utc(day.and_time(NaiveTime::MIN) + Duration::seconds(SECS_PER_DAY - 1))
}

/// The deadline the consent named for a signup accepted at `accepted_at`
/// (UTC): the end of its `deadline_on`, the local signup day + 30 days.
pub fn signup_deadline_at(accepted_at: NaiveDateTime) -> NaiveDateTime {
    end_of_local_day(deadline_on(signup_day(accepted_at.and_utc())))
}

/// `days` whole local days after `at`: the same wall time in [`SIGNUP_TZ`]
/// on the local date `days` on, as UTC, so a deadline shown as a date never
/// drifts across a DST change.
pub fn add_local_days(at: NaiveDateTime, days: i64) -> NaiveDateTime {
    local_to_utc(SIGNUP_TZ.from_utc_datetime(&at).naive_local() + Duration::days(days))
}

/// The subscription's first period boundary after `after`: `anchor` plus
/// whole months, the day of month clamped to the month's length as Stripe
/// does (anchor Jan 31: Feb 28, Mar 31, Apr 30).
pub fn period_after(anchor: NaiveDateTime, after: NaiveDateTime) -> Option<NaiveDateTime> {
    (0..=MAX_PERIODS * 2)
        .filter_map(|months| anchor.checked_add_months(Months::new(months)))
        .find(|boundary| *boundary > after)
}

/// The advance for a notice at `now`: `monthly_cents` of the live
/// subscription.
///
/// It is charged on the next invoice (`next_invoice_at`: the trial end while
/// trialing, else the current period end) when that is at least
/// [`NOTICE_DAYS`] away, else on the first later one. `anchor`:
/// `billing_cycle_anchor`, `next_invoice_at` without one.
pub fn advance_terms(
    monthly_cents: i64,
    anchor: Option<NaiveDateTime>,
    next_invoice_at: NaiveDateTime,
    now: NaiveDateTime,
) -> Option<AdvanceTerms> {
    let anchor = anchor.unwrap_or(next_invoice_at);
    let earliest = now + Duration::days(NOTICE_DAYS);
    let mut charge_at = next_invoice_at;
    for _ in 0..MAX_PERIODS {
        if charge_at >= earliest {
            break;
        }
        charge_at = period_after(anchor, charge_at)?;
    }
    if charge_at < earliest {
        return None;
    }
    let prepaid_from = period_after(anchor, charge_at)?;
    let prepaid_until = period_after(anchor, prepaid_from)?;
    Some(AdvanceTerms {
        advance_cents: monthly_cents,
        charge_at,
        prepaid_from,
        prepaid_until,
    })
}

fn unchanged(commitment: &Commitment) -> Outcome {
    Outcome {
        commitment: commitment.clone(),
        events: Vec::new(),
    }
}

/// Appends an event stamped with the commitment as it is now.
fn push(out: &mut Outcome, kind: EventKind, from: Status, actor: Option<i32>, detail: Value) {
    out.events.push(Event {
        kind,
        from: Some(from),
        to: out.commitment.status,
        deadline_at: out.commitment.deadline_at,
        actor,
        detail,
    });
}

/// The advance is on a subscription invoice: only the item's settlement
/// changes it now.
pub const fn is_invoiced(commitment: &Commitment) -> bool {
    matches!(commitment.status, Status::AdvanceScheduled) && commitment.stripe_invoice_id.is_some()
}

/// An invoice item is recorded and its month not credited back yet.
pub const fn item_open(commitment: &Commitment) -> bool {
    commitment.advance_item_id.is_some() && commitment.advance_credit_id.is_none()
}

/// Settled and invoiced commitments refuse every action.
const fn check_open(commitment: &Commitment) -> Result<(), Refusal> {
    if commitment.status.is_final() {
        Err(Refusal::Final)
    } else if is_invoiced(commitment) {
        Err(Refusal::AlreadyInvoiced)
    } else {
        Ok(())
    }
}

/// Where a live commitment rests: booked while a call is stored.
const fn resting_status(commitment: &Commitment) -> Status {
    if commitment.booking.is_some() {
        Status::Booked
    } else {
        Status::Pending
    }
}

fn terms_detail(terms: Option<&AdvanceTerms>) -> Value {
    terms.map_or(Value::Null, |t| {
        json!({
            "advance_cents": t.advance_cents,
            "charge_at": ts(t.charge_at),
            "prepaid_from": ts(t.prepaid_from),
            "prepaid_until": ts(t.prepaid_until),
        })
    })
}

/// The notice and its terms go: a new notice comes before any charge. A
/// notice or a scheduled advance rests again (the item, if any, stays
/// recorded for [`Due::SettleItem`]).
const fn clear_notice(c: &mut Commitment) {
    if matches!(c.status, Status::NoticeSent | Status::AdvanceScheduled) {
        c.status = resting_status(c);
    }
    c.notice_sent_at = None;
    c.notice_cycle_at = None;
    c.terms = None;
}

fn no_show_window_end(call: &Booking) -> NaiveDateTime {
    call.end_at + Duration::hours(NO_SHOW_WINDOW_HOURS)
}

/// A call that counts (ends by `limit`) and is still inside its no-show
/// window: the notice waits for it.
fn call_awaiting(call: Option<&Booking>, limit: NaiveDateTime, now: NaiveDateTime) -> bool {
    call.is_some_and(|call| call.end_at <= limit && now < no_show_window_end(call))
}

fn same_call(a: &Booking, b: &Booking) -> bool {
    a.invitee_uri == b.invitee_uri && a.start_at == b.start_at && a.end_at == b.end_at
}

/// Whole days in `span`, rounded up, at least one.
fn days_given_back(span: Duration) -> i64 {
    let secs = span.num_seconds() + i64::from(span.subsec_nanos() > 0);
    (secs.saturating_add(SECS_PER_DAY - 1) / SECS_PER_DAY).max(1)
}

/// The deadline moved: a sent or held notice named the old deadline, so it
/// is dropped with its terms (a sent or scheduled one goes back to booked
/// or pending; a new notice naming the new deadline, with a new charge
/// date at least 7 days on, falls due once it passes). Suspended stays
/// suspended. With the deadline in the future again the notice cycle ends
/// too.
fn withdraw_stale_notice(commitment: &mut Commitment, now: NaiveDateTime) -> bool {
    if matches!(
        commitment.status,
        Status::Suspended | Status::NoticeSent | Status::AdvanceScheduled
    ) {
        let dropped = commitment.notice_sent_at.is_some();
        clear_notice(commitment);
        return dropped;
    }
    if commitment.deadline_at > now {
        commitment.notice_cycle_at = None;
    }
    false
}

/// Adds `days` local days to the deadline; returns the old deadline and
/// whether a notice was withdrawn.
fn move_deadline(
    commitment: &mut Commitment,
    days: i64,
    now: NaiveDateTime,
) -> (NaiveDateTime, bool) {
    let old = commitment.deadline_at;
    commitment.deadline_at = add_local_days(old, days);
    (old, withdraw_stale_notice(commitment, now))
}

/// The row provisioning inserts: `pending`, deadline as frozen at signup
/// ([`signup_deadline_at`]).
pub fn new_commitment(
    company_id: i32,
    signup_at: NaiveDateTime,
    deadline_at: NaiveDateTime,
    signup_advance_cents: i64,
) -> Outcome {
    let commitment = Commitment {
        company_id,
        status: Status::Pending,
        created_at: signup_at,
        deadline_at,
        original_deadline_at: deadline_at,
        tolled_days: 0,
        extension_used: false,
        signup_advance_cents,
        booking: None,
        completed_at: None,
        completed_by: None,
        no_show_at: None,
        no_show_call_start_at: None,
        suspended_at: None,
        notice_sent_at: None,
        notice_cycle_at: None,
        terms: None,
        advance_item_id: None,
        stripe_invoice_id: None,
        advance_credit_id: None,
        review_hold_at: None,
        review_hold_by: None,
        availability_checked_on: None,
    };
    let created = Event {
        kind: EventKind::Created,
        from: None,
        to: Status::Pending,
        deadline_at,
        actor: None,
        detail: json!({
            "signup_advance_cents": signup_advance_cents,
            "deadline_at": ts(deadline_at),
        }),
    };
    Outcome {
        commitment,
        events: vec![created],
    }
}

/// The call starts at or before the missed call's start: the missed call
/// itself (Calendly still lists it as active) or an older one. A later call
/// counts, even one starting before the no-show was recorded.
fn is_missed(commitment: &Commitment, booking: &Booking) -> bool {
    commitment
        .no_show_call_start_at
        .is_some_and(|missed| booking.start_at <= missed)
}

/// Stores a newly seen booking: `Booked` when none was stored,
/// `Rescheduled` when it replaces a different one, nothing when it is the
/// stored one or a missed call. Pending becomes booked; other statuses stay.
fn store_booking(out: &mut Outcome, booking: Booking, source: &str) {
    if is_missed(&out.commitment, &booking) {
        return;
    }
    let from = out.commitment.status;
    let (kind, detail) = match out.commitment.booking.as_ref() {
        Some(old) if same_call(old, &booking) => return,
        Some(old) => (
            EventKind::Rescheduled,
            json!({
                "by": "customer",
                "source": source,
                "invitee_uri": booking.invitee_uri,
                "old_start_at": ts(old.start_at),
                "new_start_at": ts(booking.start_at),
                "new_end_at": ts(booking.end_at),
            }),
        ),
        None => (
            EventKind::Booked,
            json!({
                "source": source,
                "invitee_uri": booking.invitee_uri,
                "call_start_at": ts(booking.start_at),
                "call_end_at": ts(booking.end_at),
            }),
        ),
    };
    out.commitment.booking = Some(booking);
    if out.commitment.status == Status::Pending {
        out.commitment.status = Status::Booked;
    }
    push(out, kind, from, None, detail);
}

/// The customer booked (or rebooked) through the widget: `POST
/// /v1/onboarding-call/bookings`, after the api checked our tag on the
/// invitee.
///
/// Replaces a stored booking (`Rescheduled`). A call starting at or
/// before the missed call's start is refused. A call that ends by the
/// charge date takes a scheduled advance back at once (`call_booked`): the
/// caller then deletes the pending item ([`Due::SettleItem`]).
pub fn record_booking(
    commitment: &Commitment,
    booking: Booking,
    _now: NaiveDateTime,
) -> Result<Outcome, Refusal> {
    check_open(commitment)?;
    if is_missed(commitment, &booking) {
        return Err(Refusal::MissedCall);
    }
    let mut out = unchanged(commitment);
    store_booking(&mut out, booking, "widget");
    withdraw_for_call(&mut out);
    Ok(out)
}

/// The no-show clears the booking (`no_show_at` and the call's start keep
/// the history); booked goes back to pending.
fn record_no_show(
    out: &mut Outcome,
    call: &Booking,
    at: NaiveDateTime,
    source: &str,
    actor: Option<i32>,
) {
    let from = out.commitment.status;
    out.commitment.no_show_at = Some(at);
    out.commitment.no_show_call_start_at = Some(call.start_at);
    out.commitment.booking = None;
    if from == Status::Booked {
        out.commitment.status = Status::Pending;
    }
    let detail = json!({
        "source": source,
        "no_show_at": ts(at),
        "invitee_uri": call.invitee_uri,
        "call_start_at": ts(call.start_at),
        "call_end_at": ts(call.end_at),
    });
    push(out, EventKind::NoShow, from, actor, detail);
}

fn cancel_booking(
    out: &mut Outcome,
    call: &Booking,
    by_host: bool,
    at: NaiveDateTime,
    now: NaiveDateTime,
) {
    let from = out.commitment.status;
    out.commitment.booking = None;
    if from == Status::Booked {
        out.commitment.status = Status::Pending;
    }
    let detail = if by_host {
        let days = days_given_back(at - call.booked_at);
        let (old_deadline, withdrawn) = move_deadline(&mut out.commitment, days, now);
        json!({
            "by": "host",
            "cancelled_at": ts(at),
            "call_start_at": ts(call.start_at),
            "booked_at": ts(call.booked_at),
            "days_added": days,
            "old_deadline_at": ts(old_deadline),
            "new_deadline_at": ts(out.commitment.deadline_at),
            "notice_withdrawn": withdrawn,
        })
    } else {
        json!({
            "by": "customer",
            "cancelled_at": ts(at),
            "call_start_at": ts(call.start_at),
        })
    };
    push(out, EventKind::BookingCancelled, from, None, detail);
}

/// Step 2: what Calendly says about the booking today.
fn refresh_booking(out: &mut Outcome, check: &BookingCheck, now: NaiveDateTime) {
    let from = out.commitment.status;
    let Some(old) = out.commitment.booking.clone() else {
        if let BookingCheck::Found(found) = check {
            store_booking(out, found.clone(), "found");
        }
        return;
    };
    match check {
        BookingCheck::Active {
            no_show_at: Some(mark),
        } if *mark <= no_show_window_end(&old) => {
            record_no_show(out, &old, *mark, "calendly", None);
        }
        BookingCheck::Moved(new) if *new != old => {
            out.commitment.booking = Some(new.clone());
            let detail = json!({
                "by": "customer",
                "invitee_uri": new.invitee_uri,
                "old_start_at": ts(old.start_at),
                "new_start_at": ts(new.start_at),
                "new_end_at": ts(new.end_at),
            });
            push(out, EventKind::Rescheduled, from, None, detail);
        }
        BookingCheck::MovedByHost(new) if *new != old => {
            let days = days_given_back(new.start_at - old.start_at);
            out.commitment.booking = Some(new.clone());
            let (old_deadline, withdrawn) = move_deadline(&mut out.commitment, days, now);
            let detail = json!({
                "by": "host",
                "invitee_uri": new.invitee_uri,
                "old_start_at": ts(old.start_at),
                "new_start_at": ts(new.start_at),
                "new_end_at": ts(new.end_at),
                "days_added": days,
                "old_deadline_at": ts(old_deadline),
                "new_deadline_at": ts(out.commitment.deadline_at),
                "notice_withdrawn": withdrawn,
            });
            push(out, EventKind::Rescheduled, from, None, detail);
        }
        BookingCheck::Cancelled { by_host, at } => cancel_booking(out, &old, *by_host, *at, now),
        BookingCheck::None
        | BookingCheck::Found(_)
        | BookingCheck::Active { .. }
        | BookingCheck::Moved(_)
        | BookingCheck::MovedByHost(_)
        | BookingCheck::Unknown => {}
    }
}

/// When a call must end to count: the deadline, or once a notice is out
/// its charge date.
const fn call_limit(c: &Commitment) -> NaiveDateTime {
    match (c.status, c.terms) {
        (Status::NoticeSent | Status::AdvanceScheduled, Some(terms)) => terms.charge_at,
        _ => c.deadline_at,
    }
}

/// Step 3: the stored call ended in time and its no-show window is over.
fn auto_complete(out: &mut Outcome, now: NaiveDateTime) -> bool {
    let c = &out.commitment;
    let Some(call) = c.booking.as_ref() else {
        return false;
    };
    let counts = match c.status {
        Status::Pending | Status::Booked | Status::NoticeSent | Status::AdvanceScheduled => {
            call.end_at <= call_limit(c)
        }
        Status::Suspended => true,
        _ => false,
    };
    if !counts || now < no_show_window_end(call) {
        return false;
    }
    let from = c.status;
    let detail = json!({
        "basis": "call",
        "invitee_uri": call.invitee_uri,
        "call_start_at": ts(call.start_at),
        "call_end_at": ts(call.end_at),
    });
    out.commitment.status = Status::Completed;
    out.commitment.completed_at = Some(now);
    out.commitment.completed_by = None;
    push(out, EventKind::Completed, from, None, detail);
    true
}

/// Whether a cancellation request made at `requested` suspends (Terms
/// 11.6): any request while no notice was sent (before or after the
/// deadline), and one before the charge date while a notice is out.
pub fn request_suspends(c: &Commitment, requested: NaiveDateTime) -> bool {
    match c.status {
        Status::Pending | Status::Booked => true,
        Status::NoticeSent | Status::AdvanceScheduled => {
            c.terms.is_none_or(|terms| requested < terms.charge_at)
        }
        _ => false,
    }
}

/// What withdrawing a cancellation request at `withdrawn_at` resumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resumption {
    /// Before the deadline: the countdown (a notice is dropped).
    Countdown,
    /// The notice sent after the deadline stands: its charge date is still
    /// at least [`NOTICE_DAYS`] away.
    KeepsCharge(NaiveDateTime),
    /// A new notice comes first, naming the next charge at least
    /// [`NOTICE_DAYS`] after it.
    NewNotice,
}

/// The withdrawal rule (Terms 11.6), shared by the daily run and the
/// withdrawal email.
pub fn resumption(c: &Commitment, withdrawn_at: NaiveDateTime) -> Resumption {
    if withdrawn_at < c.deadline_at {
        return Resumption::Countdown;
    }
    match (c.notice_sent_at, c.terms) {
        // Only a notice sent after the current deadline passed is about it.
        (Some(sent), Some(terms))
            if sent >= c.deadline_at
                && terms.charge_at - withdrawn_at >= Duration::days(NOTICE_DAYS) =>
        {
            Resumption::KeepsCharge(terms.charge_at)
        }
        _ => Resumption::NewNotice,
    }
}

/// A suspension withdrawn at `withdrawn_at` resumes ([`resumption`]).
fn resume(c: &mut Commitment, withdrawn_at: NaiveDateTime) -> &'static str {
    c.suspended_at = None;
    c.notice_cycle_at = None;
    match resumption(c, withdrawn_at) {
        Resumption::KeepsCharge(_) => {
            c.status = Status::NoticeSent;
            "notice_sent"
        }
        Resumption::Countdown => {
            clear_notice(c);
            c.status = resting_status(c);
            "countdown"
        }
        Resumption::NewNotice => {
            clear_notice(c);
            c.status = resting_status(c);
            "notice"
        }
    }
}

/// Suspends for a request made at `requested`; the notice and its terms are
/// kept for [`resume`], its email cycle ends. A scheduled item is left for
/// [`Due::SettleItem`] to delete.
fn suspend(out: &mut Outcome, requested: NaiveDateTime, now: NaiveDateTime, mut detail: Value) {
    let from = out.commitment.status;
    let c = &mut out.commitment;
    c.status = Status::Suspended;
    c.suspended_at = Some(now);
    c.notice_cycle_at = None;
    detail["requested_at"] = json!(ts(requested));
    detail["notice_held"] = json!(c.notice_sent_at.is_some());
    push(out, EventKind::Suspended, from, None, detail);
}

/// When the latest withdrawn request was withdrawn, if one was since the
/// notice.
fn last_withdrawal(facts: &DailyFacts) -> Option<NaiveDateTime> {
    facts
        .withdrawn_cancellations
        .iter()
        .map(|(_, withdrawn)| *withdrawn)
        .max()
}

/// A request made during the notice period and withdrawn less than
/// [`NOTICE_DAYS`] before the charge with no run in between (never
/// suspended): the notice is dropped as [`resume`] drops it, and a new one
/// falls due.
fn withdrawn_late(out: &mut Outcome, facts: &DailyFacts) {
    let c = &out.commitment;
    let (Status::NoticeSent | Status::AdvanceScheduled, Some(sent), Some(terms)) =
        (c.status, c.notice_sent_at, c.terms)
    else {
        return;
    };
    let charge_at = terms.charge_at;
    let Some(&(requested, withdrawn)) =
        facts
            .withdrawn_cancellations
            .iter()
            .find(|(requested, withdrawn)| {
                *requested < charge_at
                    && *withdrawn > sent
                    && charge_at - *withdrawn < Duration::days(NOTICE_DAYS)
            })
    else {
        return;
    };
    let from = c.status;
    clear_notice(&mut out.commitment);
    let detail = json!({
        "resumes_at": "notice",
        "charge_at": null,
        "requested_at": ts(requested),
        "withdrawn_at": ts(withdrawn),
    });
    push(out, EventKind::Resumed, from, None, detail);
}

/// Step 4: a cancellation request made in time ([`request_suspends`])
/// suspends; its withdrawal resumes ([`resume`], at the latest withdrawal's
/// time, else `now`). Returns whether the run goes on.
fn suspension(out: &mut Outcome, facts: &DailyFacts, now: NaiveDateTime) -> bool {
    let from = out.commitment.status;
    match (from, facts.cancellation_requested_at) {
        (Status::Suspended, Some(_)) => false,
        (Status::Suspended, None) => {
            let withdrawn_at = last_withdrawal(facts).unwrap_or(now);
            let resumes_at = resume(&mut out.commitment, withdrawn_at);
            let c = &out.commitment;
            let detail = json!({
                "resumes_at": resumes_at,
                "charge_at": c.terms.map(|terms| ts(terms.charge_at)),
            });
            push(out, EventKind::Resumed, from, None, detail);
            true
        }
        (_, Some(requested)) if request_suspends(&out.commitment, requested) => {
            suspend(out, requested, now, json!({}));
            false
        }
        _ => true,
    }
}

/// One day's tolling decision; `NotChecked` tolls only for an earlier day
/// (the job missed it).
fn toll_day(out: &mut Outcome, day: NaiveDate, availability: Availability, earlier: bool) {
    let c = &mut out.commitment;
    let reason = match availability {
        Availability::NotChecked if earlier => "not_checked",
        Availability::NotChecked => return,
        Availability::Available => {
            c.availability_checked_on = Some(day);
            return;
        }
        Availability::NoSlots => "no_slots",
        Availability::NotConfigured => "not_configured",
        Availability::Error => "error",
    };
    let from = c.status;
    let old = c.deadline_at;
    c.deadline_at = add_local_days(old, 1);
    c.tolled_days = c.tolled_days.saturating_add(1);
    c.availability_checked_on = Some(day);
    let detail = json!({
        "reason": reason,
        "day": day.format("%Y-%m-%d").to_string(),
        "old_deadline_at": ts(old),
        "new_deadline_at": ts(c.deadline_at),
    });
    push(out, EventKind::Tolled, from, None, detail);
}

/// Step 5: each local day with no bookable slot moves the deadline by one
/// day, once: the earlier days the job missed, then today. Days are the
/// buyer's (`SIGNUP_TZ`), like the deadline and the business days.
fn toll(out: &mut Outcome, facts: &DailyFacts, now: NaiveDateTime) {
    let today = local_day(now);
    for (day, availability) in &facts.earlier_days {
        let c = &out.commitment;
        if !matches!(c.status, Status::Pending | Status::Booked)
            || *day >= today
            || start_of_local_day(*day) >= c.deadline_at
            || c.availability_checked_on
                .is_some_and(|checked| checked >= *day)
        {
            continue;
        }
        toll_day(out, *day, *availability, true);
    }
    let c = &out.commitment;
    if !matches!(c.status, Status::Pending | Status::Booked)
        || now >= c.deadline_at
        || c.availability_checked_on == Some(today)
    {
        return;
    }
    toll_day(out, today, facts.availability, false);
}

/// Why the advance may not be charged at all: skipped for good.
const fn skip_reason(facts: &DailyFacts) -> Option<&'static str> {
    if !facts.charging_enabled {
        Some("charging_off")
    } else if !facts.consent_on_file {
        Some("no_consent")
    } else {
        None
    }
}

fn skip(out: &mut Outcome, reason: &str) {
    let from = out.commitment.status;
    out.commitment.status = Status::AdvanceSkipped;
    let detail = json!({
        "reason": reason,
        "advance_cents": out.commitment.terms.map(|t| t.advance_cents),
    });
    push(out, EventKind::AdvanceSkipped, from, None, detail);
}

fn void(out: &mut Outcome, detail: Value) {
    let from = out.commitment.status;
    out.commitment.status = Status::Void;
    push(out, EventKind::Voided, from, None, detail);
}

/// A booked call (not the missed one) that ends by the charge date: it
/// still counts, so no item waits on the invoice for it.
fn call_before_charge(c: &Commitment, charge_at: NaiveDateTime) -> bool {
    c.booking
        .as_ref()
        .is_some_and(|call| call.end_at <= charge_at && !is_missed(c, call))
}

/// A notice is out and the booked call ends by its charge date: no advance
/// item may be (or stay) in Stripe ([`Withdrawal::CallBooked`]).
pub fn call_avoids_advance(c: &Commitment) -> bool {
    matches!(c.status, Status::NoticeSent | Status::AdvanceScheduled)
        && c.terms
            .is_some_and(|terms| call_before_charge(c, terms.charge_at))
}

/// A scheduled advance whose booked call ends by the charge goes back to
/// `notice_sent` (`call_booked`); its item stays recorded for
/// [`Due::SettleItem`] to delete.
fn withdraw_for_call(out: &mut Outcome) {
    let c = &out.commitment;
    if c.status != Status::AdvanceScheduled || !call_avoids_advance(c) {
        return;
    }
    let from = c.status;
    out.commitment.status = Status::NoticeSent;
    let detail = json!({
        "reason": Withdrawal::CallBooked.as_str(),
        "item_id": out.commitment.advance_item_id,
    });
    push(out, EventKind::AdvanceWithdrawn, from, None, detail);
}

/// Steps 6 and 7: past the deadline the notice falls due, within
/// [`ITEM_WINDOW_DAYS`] of the charge the item. Either is skipped for good
/// when charging is off or no consent is on file; neither while held for
/// review; the notice waits for a call inside its no-show window, the item
/// for a call booked to end by the charge.
fn advance_due(out: &mut Outcome, facts: &DailyFacts, now: NaiveDateTime) -> Option<Due> {
    let c = &out.commitment;
    let held = c.review_hold_at.is_some();
    match (c.status, c.terms) {
        (Status::Pending | Status::Booked, _) => {
            if now < c.deadline_at || call_awaiting(c.booking.as_ref(), c.deadline_at, now) || held
            {
                return None;
            }
            if let Some(reason) = skip_reason(facts) {
                skip(out, reason);
                return None;
            }
            Some(Due::Notice)
        }
        (Status::NoticeSent, Some(terms)) => {
            if held
                || call_before_charge(c, terms.charge_at)
                || now < terms.charge_at - Duration::days(ITEM_WINDOW_DAYS)
            {
                return None;
            }
            if let Some(reason) = skip_reason(facts) {
                skip(out, reason);
                return None;
            }
            Some(Due::AddItem)
        }
        (Status::NoticeSent, None) => {
            // A notice without terms states no charge: a new one comes.
            let from = c.status;
            clear_notice(&mut out.commitment);
            let detail = json!({ "reason": "no_terms" });
            push(out, EventKind::NoticeWithdrawn, from, None, detail);
            (now >= out.commitment.deadline_at).then_some(Due::Notice)
        }
        (Status::AdvanceScheduled, Some(_)) => {
            withdraw_for_call(out);
            None
        }
        _ => None,
    }
}

/// Step 8: the latest reminder whose day has come, until the next one's.
fn reminder_due(c: &Commitment, now: NaiveDateTime) -> Option<ReminderStep> {
    if !matches!(c.status, Status::Pending | Status::Booked) || now >= c.deadline_at {
        return None;
    }
    let starts = |step: ReminderStep| c.created_at + Duration::days(step.days());
    let index = REMINDER_STEPS
        .iter()
        .rposition(|step| starts(*step) <= now)?;
    let until = REMINDER_STEPS
        .get(index + 1)
        .map_or(c.deadline_at, |next| starts(*next));
    (now < until).then_some(REMINDER_STEPS[index])
}

fn run_daily(out: &mut Outcome, facts: &DailyFacts, now: NaiveDateTime) -> Vec<Due> {
    let from = out.commitment.status;
    if from.is_final() || is_invoiced(&out.commitment) {
        return Vec::new();
    }
    if facts.subscription_ended {
        void(out, json!({ "reason": "subscription_ended" }));
        return Vec::new();
    }
    if now >= out.commitment.deadline_at && !facts.subscription_live {
        // Never a notice or a charge without a running subscription.
        void(out, json!({ "reason": "no_subscription" }));
        return Vec::new();
    }
    refresh_booking(out, &facts.booking, now);
    if auto_complete(out, now) {
        return Vec::new();
    }
    withdrawn_late(out, facts);
    if !suspension(out, facts, now) {
        return Vec::new();
    }
    toll(out, facts, now);
    if out.commitment.deadline_at > now {
        out.commitment.notice_cycle_at = None;
    }
    if let Some(due) = advance_due(out, facts, now) {
        if due == Due::Notice && out.commitment.notice_cycle_at.is_none() {
            out.commitment.notice_cycle_at = Some(now);
        }
        return vec![due];
    }
    reminder_due(&out.commitment, now)
        .map(Due::Reminder)
        .into_iter()
        .collect()
}

/// The daily run for one company.
///
/// Booking refresh, void (the subscription ended, or none runs at the
/// deadline), auto-complete, suspension and its withdrawal, tolling, the
/// deadline (notice or skip), the item window, reminders; first of all the
/// settlement of a recorded item. A notice falling due starts its cycle
/// (`notice_cycle_at`); save the outcome before sending it.
pub fn daily(commitment: &Commitment, facts: &DailyFacts, now: NaiveDateTime) -> Daily {
    let mut outcome = unchanged(commitment);
    let mut due = run_daily(&mut outcome, facts, now);
    if item_open(&outcome.commitment) {
        due.insert(0, Due::SettleItem);
    }
    Daily { outcome, due }
}

/// The terms the notice of the cycle states, fixed before it is sent. Only
/// while the notice is due (pending or booked, the cycle started).
pub fn notice_terms(commitment: &Commitment, terms: AdvanceTerms) -> Outcome {
    let mut out = unchanged(commitment);
    if matches!(commitment.status, Status::Pending | Status::Booked)
        && commitment.notice_cycle_at.is_some()
    {
        out.commitment.terms = Some(terms);
    }
    out
}

/// Void for `reason` (Stripe has no running subscription to charge): from
/// any status that is not settled.
pub fn voided(commitment: &Commitment, reason: &str) -> Outcome {
    let mut out = unchanged(commitment);
    if !commitment.status.is_final() && !is_invoiced(commitment) {
        void(&mut out, json!({ "reason": reason }));
    }
    out
}

/// The notice went out to at least one manager (with its terms).
pub fn notice_sent(commitment: &Commitment, now: NaiveDateTime) -> Outcome {
    let mut out = unchanged(commitment);
    let from = commitment.status;
    if !matches!(from, Status::Pending | Status::Booked) || commitment.terms.is_none() {
        return out;
    }
    out.commitment.status = Status::NoticeSent;
    out.commitment.notice_sent_at = Some(now);
    let mut detail = terms_detail(commitment.terms.as_ref());
    detail["notice_sent_at"] = json!(ts(now));
    push(&mut out, EventKind::NoticeSent, from, None, detail);
    out
}

/// The notice is dropped (its charge date passed with no item, or the
/// subscription's charge moved): a new notice with the next eligible charge
/// date falls due once the deadline has passed.
pub fn drop_notice(commitment: &Commitment, reason: &str, _now: NaiveDateTime) -> Outcome {
    let mut out = unchanged(commitment);
    let from = commitment.status;
    if !matches!(from, Status::NoticeSent | Status::AdvanceScheduled) {
        return out;
    }
    let detail = json!({
        "reason": reason,
        "charge_at": commitment.terms.map(|t| ts(t.charge_at)),
        "item_id": commitment.advance_item_id,
    });
    clear_notice(&mut out.commitment);
    push(&mut out, EventKind::NoticeWithdrawn, from, None, detail);
    out
}

/// The advance's invoice item is in Stripe (`invoice_id` when it was found
/// already on an invoice). Only from `notice_sent` without an item.
pub fn advance_scheduled(
    commitment: &Commitment,
    item_id: &str,
    invoice_id: Option<&str>,
    _now: NaiveDateTime,
) -> Outcome {
    let mut out = unchanged(commitment);
    let from = commitment.status;
    if from != Status::NoticeSent || commitment.advance_item_id.is_some() {
        return out;
    }
    out.commitment.status = Status::AdvanceScheduled;
    out.commitment.advance_item_id = Some(item_id.to_string());
    out.commitment.stripe_invoice_id = invoice_id.map(str::to_string);
    let mut detail = terms_detail(commitment.terms.as_ref());
    detail["item_id"] = json!(item_id);
    detail["invoice_id"] = json!(invoice_id);
    push(&mut out, EventKind::AdvanceScheduled, from, None, detail);
    out
}

/// An advance item found on a paid invoice that was never recorded (a lost
/// save).
///
/// Recorded on any commitment without an item or a credit, so it is settled
/// (credited) like a recorded one. `notice_sent` becomes `advance_scheduled`.
pub fn item_recorded(
    commitment: &Commitment,
    item_id: &str,
    invoice_id: Option<&str>,
    now: NaiveDateTime,
) -> Outcome {
    if commitment.status == Status::NoticeSent {
        return advance_scheduled(commitment, item_id, invoice_id, now);
    }
    let mut out = unchanged(commitment);
    if commitment.advance_item_id.is_some() || commitment.advance_credit_id.is_some() {
        return out;
    }
    let from = commitment.status;
    out.commitment.advance_item_id = Some(item_id.to_string());
    out.commitment.stripe_invoice_id = invoice_id.map(str::to_string);
    let detail = json!({ "found": true, "item_id": item_id, "invoice_id": invoice_id });
    push(&mut out, EventKind::AdvanceInvoiced, from, None, detail);
    out
}

/// Stripe swept the item into `invoice_id`.
pub fn item_invoiced(commitment: &Commitment, invoice_id: &str, _now: NaiveDateTime) -> Outcome {
    let mut out = unchanged(commitment);
    if !item_open(commitment) || commitment.stripe_invoice_id.as_deref() == Some(invoice_id) {
        return out;
    }
    let from = commitment.status;
    out.commitment.stripe_invoice_id = Some(invoice_id.to_string());
    let detail = json!({ "invoice_id": invoice_id, "item_id": commitment.advance_item_id });
    push(&mut out, EventKind::AdvanceInvoiced, from, None, detail);
    out
}

/// The item is out of Stripe before any charge (we deleted it, or it is
/// gone): it is forgotten.
///
/// A scheduled advance whose item vanished goes back to `notice_sent` (the
/// next run adds a new one in the window).
pub fn item_withdrawn(commitment: &Commitment, reason: &str, _now: NaiveDateTime) -> Outcome {
    let mut out = unchanged(commitment);
    let Some(item_id) = commitment.advance_item_id.clone() else {
        return out;
    };
    let from = commitment.status;
    let c = &mut out.commitment;
    c.advance_item_id = None;
    c.stripe_invoice_id = None;
    if from == Status::AdvanceScheduled {
        c.status = Status::NoticeSent;
    }
    let detail = json!({ "reason": reason, "item_id": item_id });
    push(&mut out, EventKind::AdvanceWithdrawn, from, None, detail);
    out
}

/// The invoice with the item was paid and the month credited back
/// (`credit_id`): `advance_paid`.
///
/// So no second advance ever follows. A completed or waived commitment
/// (Stripe swept the item before it could be deleted) keeps its status and
/// records the credit.
pub fn credited(
    commitment: &Commitment,
    invoice_id: &str,
    credit_id: &str,
    _now: NaiveDateTime,
) -> Outcome {
    let mut out = unchanged(commitment);
    if commitment.advance_credit_id.is_some() {
        return out;
    }
    let from = commitment.status;
    let c = &mut out.commitment;
    c.advance_credit_id = Some(credit_id.to_string());
    c.stripe_invoice_id = Some(invoice_id.to_string());
    if !matches!(from, Status::Completed | Status::Waived) {
        c.status = Status::AdvancePaid;
    }
    let mut detail = terms_detail(commitment.terms.as_ref());
    detail["invoice_id"] = json!(invoice_id);
    detail["credit_id"] = json!(credit_id);
    detail["item_id"] = json!(commitment.advance_item_id);
    push(&mut out, EventKind::AdvancePaid, from, None, detail);
    out
}

/// The invoice with the item ended unpaid (`void` or `uncollectible`):
/// nothing was paid in advance, so nothing is credited; a scheduled advance
/// is void. The item is forgotten.
pub fn invoice_ended(
    commitment: &Commitment,
    invoice_id: &str,
    invoice_status: &str,
    _now: NaiveDateTime,
) -> Outcome {
    let mut out = unchanged(commitment);
    if !item_open(commitment) {
        return out;
    }
    let from = commitment.status;
    let item_id = out.commitment.advance_item_id.take();
    if from == Status::AdvanceScheduled {
        out.commitment.status = Status::Void;
    }
    let detail = json!({
        "reason": format!("invoice_{invoice_status}"),
        "invoice_id": invoice_id,
        "item_id": item_id,
    });
    push(&mut out, EventKind::Voided, from, None, detail);
    out
}

/// The subscription ended while the item sat on its unpaid invoice.
///
/// The advance line was credited off that invoice (`credit_note_id`), as
/// the month it paid for never comes. The item is forgotten; a live
/// commitment is void.
pub fn item_credited_off(
    commitment: &Commitment,
    invoice_id: &str,
    credit_note_id: &str,
    _now: NaiveDateTime,
) -> Outcome {
    let mut out = unchanged(commitment);
    if !item_open(commitment) {
        return out;
    }
    let from = commitment.status;
    let item_id = out.commitment.advance_item_id.take();
    out.commitment.stripe_invoice_id = Some(invoice_id.to_string());
    if !from.is_final() {
        out.commitment.status = Status::Void;
    }
    let detail = json!({
        "reason": "subscription_ended",
        "invoice_id": invoice_id,
        "item_id": item_id,
        "credit_note_id": credit_note_id,
    });
    push(&mut out, EventKind::Voided, from, None, detail);
    out
}

/// What is checked right before the item reaches Stripe, and again while it
/// is pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChargeFacts {
    /// `ONBOARDING_ADVANCE_CHARGING=1`.
    pub charging_enabled: bool,
    /// The company accepted the `onboarding_advance` consent.
    pub consent_on_file: bool,
    /// As in [`DailyFacts::cancellation_requested_at`].
    pub cancellation_requested_at: Option<NaiveDateTime>,
    /// The live subscription runs (trialing, active, past due).
    pub subscription_live: bool,
    /// Stripe has the subscription set to end (`cancel_at_period_end` or
    /// `cancel_at`).
    pub cancel_scheduled: bool,
    /// The live subscription's next invoice: the trial end while trialing,
    /// else the current period end.
    pub next_charge_at: Option<NaiveDateTime>,
}

/// Why the advance may not be (or stay) in Stripe now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Withdrawal {
    /// No running subscription: void.
    SubscriptionEnded,
    /// A cancellation request made before the charge date: suspended.
    CancellationRequested(NaiveDateTime),
    /// Stripe ends the subscription with no request recorded yet (the
    /// webhook sync records one): waits as `notice_sent`.
    CancelScheduled,
    /// A booked call ends by the charge date (it counts once held): waits
    /// as `notice_sent`.
    CallBooked,
    /// Held for review: waits as `notice_sent`.
    Held,
    /// `ONBOARDING_ADVANCE_CHARGING` is off: skipped for good.
    ChargingOff,
    /// No `onboarding_advance` consent on file: skipped for good.
    NoConsent,
    /// The subscription's next charge is not the notice's date: a new
    /// notice.
    ChargeMoved,
}

impl Withdrawal {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SubscriptionEnded => "subscription_ended",
            Self::CancellationRequested(_) => "cancellation_requested",
            Self::CancelScheduled => "cancel_scheduled",
            Self::CallBooked => "call_booked",
            Self::Held => "review_hold",
            Self::ChargingOff => "charging_off",
            Self::NoConsent => "no_consent",
            Self::ChargeMoved => "charge_moved",
        }
    }
}

/// Why the advance of `commitment` (`notice_sent` or `advance_scheduled`
/// with a pending item) may not be in Stripe now, in this order; `None`
/// when it may.
pub fn withdrawal(commitment: &Commitment, facts: &ChargeFacts) -> Option<Withdrawal> {
    let charge_at = commitment.terms.map(|terms| terms.charge_at);
    if !facts.subscription_live {
        Some(Withdrawal::SubscriptionEnded)
    } else if let Some(requested) = facts.cancellation_requested_at
        && request_suspends(commitment, requested)
    {
        Some(Withdrawal::CancellationRequested(requested))
    } else if facts.cancel_scheduled {
        Some(Withdrawal::CancelScheduled)
    } else if call_avoids_advance(commitment) {
        Some(Withdrawal::CallBooked)
    } else if commitment.review_hold_at.is_some() {
        Some(Withdrawal::Held)
    } else if !facts.charging_enabled {
        Some(Withdrawal::ChargingOff)
    } else if !facts.consent_on_file {
        Some(Withdrawal::NoConsent)
    } else if charge_at.is_none()
        || facts
            .next_charge_at
            .is_some_and(|next| Some(next) != charge_at)
    {
        Some(Withdrawal::ChargeMoved)
    } else {
        None
    }
}

/// The advance may not be in Stripe ([`withdrawal`]).
///
/// Void, suspended, skipped for good, a new notice, or (a scheduled
/// cancellation, a call booked before the charge, a review hold) back to
/// `notice_sent` to wait. A pending
/// item stays recorded: delete it, then [`item_withdrawn`]. From
/// `notice_sent` or `advance_scheduled` only.
pub fn advance_withdrawn(
    commitment: &Commitment,
    reason: Withdrawal,
    now: NaiveDateTime,
) -> Outcome {
    let mut out = unchanged(commitment);
    let from = commitment.status;
    if !matches!(from, Status::NoticeSent | Status::AdvanceScheduled) {
        return out;
    }
    let detail = json!({ "reason": reason.as_str(), "item_id": commitment.advance_item_id });
    match reason {
        Withdrawal::SubscriptionEnded => void(&mut out, detail),
        Withdrawal::CancellationRequested(requested) => suspend(&mut out, requested, now, detail),
        Withdrawal::ChargingOff => skip(&mut out, "charging_off"),
        Withdrawal::NoConsent => skip(&mut out, "no_consent"),
        Withdrawal::ChargeMoved => return drop_notice(commitment, reason.as_str(), now),
        Withdrawal::CancelScheduled | Withdrawal::CallBooked | Withdrawal::Held => {
            if from == Status::AdvanceScheduled {
                out.commitment.status = Status::NoticeSent;
                push(&mut out, EventKind::AdvanceWithdrawn, from, None, detail);
            }
        }
    }
    out
}

/// When a cancellation requested at `now` ends the subscription.
///
/// The end of the month the advance paid for while its credit is unused
/// (the renewal it pays has not come yet); `None` for the usual period end.
pub fn prepaid_cancel_at(commitment: &Commitment, now: NaiveDateTime) -> Option<NaiveDateTime> {
    let terms = commitment.terms?;
    (commitment.advance_credit_id.is_some() && now < terms.prepaid_from)
        .then_some(terms.prepaid_until)
}

/// A superuser's action, stamped with `actor`.
pub fn superuser_action(
    commitment: &Commitment,
    action: Action,
    actor: i32,
    now: NaiveDateTime,
) -> Result<Outcome, Refusal> {
    check_open(commitment)?;
    let mut out = unchanged(commitment);
    let from = commitment.status;
    let by = Some(actor);
    match action {
        Action::Extend => {
            if commitment.extension_used {
                return Err(Refusal::AlreadyExtended);
            }
            if add_local_days(commitment.deadline_at, EXTENSION_DAYS) <= now {
                return Err(Refusal::ExtensionStillPast);
            }
            out.commitment.extension_used = true;
            let (old, withdrawn) = move_deadline(&mut out.commitment, EXTENSION_DAYS, now);
            let detail = json!({
                "old_deadline_at": ts(old),
                "new_deadline_at": ts(out.commitment.deadline_at),
                "days_added": EXTENSION_DAYS,
                "notice_withdrawn": withdrawn,
            });
            push(&mut out, EventKind::Extended, from, by, detail);
        }
        Action::Waive => {
            out.commitment.status = Status::Waived;
            let detail = json!({
                "by": "superuser",
                "advance_cents": commitment.terms.map(|t| t.advance_cents),
            });
            push(&mut out, EventKind::Waived, from, by, detail);
        }
        Action::Complete => {
            out.commitment.status = Status::Completed;
            out.commitment.completed_at = Some(now);
            out.commitment.completed_by = by;
            push(
                &mut out,
                EventKind::Completed,
                from,
                by,
                json!({ "basis": "superuser" }),
            );
        }
        Action::NoShow => {
            let Some(call) = commitment.booking.as_ref() else {
                return Err(Refusal::NotBooked);
            };
            if now < call.start_at {
                return Err(Refusal::CallNotStarted);
            }
            if now > no_show_window_end(call) {
                return Err(Refusal::NoShowTooLate);
            }
            record_no_show(&mut out, call, now, "superuser", by);
        }
        Action::Hold => {
            if commitment.review_hold_at.is_some() {
                return Err(Refusal::AlreadyHeld);
            }
            let c = &mut out.commitment;
            c.review_hold_at = Some(now);
            c.review_hold_by = by;
            if from == Status::AdvanceScheduled {
                c.status = Status::NoticeSent;
            }
            let detail = json!({ "item_id": commitment.advance_item_id });
            push(&mut out, EventKind::ReviewHeld, from, by, detail);
        }
        Action::Release => {
            if commitment.review_hold_at.is_none() {
                return Err(Refusal::NotHeld);
            }
            let c = &mut out.commitment;
            c.review_hold_at = None;
            c.review_hold_by = None;
            // Fewer than 7 days to the charge: a new notice names the next
            // eligible one.
            let renotice = from == Status::NoticeSent
                && c.terms
                    .is_none_or(|t| t.charge_at - now < Duration::days(NOTICE_DAYS));
            if renotice {
                clear_notice(c);
            }
            let detail = json!({ "new_notice": renotice });
            push(&mut out, EventKind::ReviewReleased, from, by, detail);
        }
    }
    Ok(out)
}

/// The actions a superuser may take now (the page shows only these).
pub fn allowed_actions(commitment: &Commitment, now: NaiveDateTime) -> Vec<Action> {
    ACTIONS
        .into_iter()
        .filter(|action| superuser_action(commitment, *action, 0, now).is_ok())
        .collect()
}

/// The deadline the extension would give, while it is allowed.
pub fn extension_deadline(commitment: &Commitment, now: NaiveDateTime) -> Option<NaiveDateTime> {
    superuser_action(commitment, Action::Extend, 0, now)
        .ok()
        .map(|out| out.commitment.deadline_at)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACTOR: i32 = 9;
    const ALL: [Status; 10] = [
        Status::Pending,
        Status::Booked,
        Status::Completed,
        Status::Waived,
        Status::Suspended,
        Status::NoticeSent,
        Status::AdvanceScheduled,
        Status::AdvancePaid,
        Status::AdvanceSkipped,
        Status::Void,
    ];
    const SETTLED: [Status; 5] = [
        Status::Completed,
        Status::Waived,
        Status::AdvancePaid,
        Status::AdvanceSkipped,
        Status::Void,
    ];

    fn at(text: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").unwrap()
    }

    fn date(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
    }

    fn secs(text: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").unwrap()
    }

    /// Signed up 2026-10-01 10:00, deadline 2026-10-31 10:00 (06:00 EDT).
    /// Moved past Nov 1, the same local time is 11:00 UTC (EST).
    fn base() -> Commitment {
        new_commitment(7, at("2026-10-01 10:00"), at("2026-10-31 10:00"), 30_000).commitment
    }

    fn with_status(status: Status) -> Commitment {
        Commitment { status, ..base() }
    }

    /// A one-hour call, recorded 2026-10-02 10:00.
    fn call(start: &str) -> Booking {
        let start_at = at(start);
        Booking {
            invitee_uri: format!("https://api.calendly.com/scheduled_events/E/invitees/{start}"),
            event_uri: "https://api.calendly.com/scheduled_events/E".to_string(),
            start_at,
            end_at: start_at + Duration::hours(1),
            reschedule_url: None,
            cancel_url: None,
            booked_at: at("2026-10-02 10:00"),
        }
    }

    fn book(commitment: &Commitment, booking: Booking) -> Commitment {
        record_booking(commitment, booking, at("2026-10-02 10:00"))
            .unwrap()
            .commitment
    }

    fn booked(start: &str) -> Commitment {
        book(&base(), call(start))
    }

    /// $300 a month, first charge at the trial end 2027-01-01 10:00: the
    /// advance pays for Feb 1 to Mar 1.
    fn terms() -> AdvanceTerms {
        AdvanceTerms {
            advance_cents: 30_000,
            charge_at: at("2027-01-01 10:00"),
            prepaid_from: at("2027-02-01 10:00"),
            prepaid_until: at("2027-03-01 10:00"),
        }
    }

    /// The notice of 2026-10-31 12:00 with [`terms`].
    fn noticed(commitment: &Commitment) -> Commitment {
        let due = Commitment {
            notice_cycle_at: Some(at("2026-10-31 11:00")),
            ..commitment.clone()
        };
        let fixed = notice_terms(&due, terms()).commitment;
        notice_sent(&fixed, at("2026-10-31 12:00")).commitment
    }

    /// The item `ii_1` added on 2026-12-29 10:00.
    fn scheduled() -> Commitment {
        advance_scheduled(&noticed(&base()), "ii_1", None, at("2026-12-29 10:00")).commitment
    }

    const fn facts() -> DailyFacts {
        DailyFacts {
            booking: BookingCheck::None,
            availability: Availability::NotChecked,
            earlier_days: Vec::new(),
            cancellation_requested_at: None,
            withdrawn_cancellations: Vec::new(),
            subscription_ended: false,
            subscription_live: true,
            charging_enabled: true,
            consent_on_file: true,
        }
    }

    fn with(booking: BookingCheck) -> DailyFacts {
        let mut facts = facts();
        facts.booking = booking;
        facts
    }

    fn requested(at_text: &str) -> DailyFacts {
        DailyFacts {
            cancellation_requested_at: Some(at(at_text)),
            ..facts()
        }
    }

    fn run(commitment: &Commitment, facts: &DailyFacts, now: &str) -> Daily {
        daily(commitment, facts, at(now))
    }

    fn due_at(commitment: &Commitment, now: &str) -> Vec<Due> {
        run(commitment, &facts(), now).due
    }

    fn kinds(out: &Outcome) -> Vec<EventKind> {
        out.events.iter().map(|event| event.kind).collect()
    }

    /// Unchanged but for the notice cycle the run started at `now`.
    fn cycle_started(commitment: &Commitment, now: &str) -> Outcome {
        unchanged(&Commitment {
            notice_cycle_at: Some(at(now)),
            ..commitment.clone()
        })
    }

    /// Booked for 2026-10-20 15:00, marked a no-show by a superuser at 15:30.
    fn missed() -> Commitment {
        let c = booked("2026-10-20 15:00");
        superuser_action(&c, Action::NoShow, ACTOR, at("2026-10-20 15:30"))
            .unwrap()
            .commitment
    }

    const fn charge_facts() -> ChargeFacts {
        ChargeFacts {
            charging_enabled: true,
            consent_on_file: true,
            cancellation_requested_at: None,
            subscription_live: true,
            cancel_scheduled: false,
            next_charge_at: None,
        }
    }

    // Strings

    #[test]
    fn statuses_round_trip_five_are_final_and_old_fee_values_still_read() {
        for status in ALL {
            assert_eq!(Status::parse(status.as_str()), Some(status));
        }
        let finals: Vec<Status> = ALL.into_iter().filter(|s| s.is_final()).collect();
        assert_eq!(finals, SETTLED);
        assert_eq!(Status::parse("Pending"), None);
        assert_eq!(Status::parse("fee_skipped"), Some(Status::AdvanceSkipped));
        for old in ["fee_invoiced", "fee_paid", "fee_failed"] {
            assert_eq!(Status::parse(old), Some(Status::Void));
        }
        assert!(ALL.iter().all(|s| s.as_str().len() <= 32));
    }

    #[test]
    fn event_kinds_reminder_steps_and_refusals_have_their_strings() {
        assert_eq!(EventKind::AdvanceScheduled.as_str(), "advance_scheduled");
        assert_eq!(EventKind::ReviewHeld.as_str(), "review_held");
        assert_eq!(
            REMINDER_STEPS.map(ReminderStep::as_str),
            ["day_7", "day_21", "day_27"]
        );
        for refusal in [
            Refusal::Final,
            Refusal::AlreadyInvoiced,
            Refusal::AlreadyExtended,
            Refusal::ExtensionStillPast,
            Refusal::NotBooked,
            Refusal::CallNotStarted,
            Refusal::NoShowTooLate,
            Refusal::MissedCall,
            Refusal::AlreadyHeld,
            Refusal::NotHeld,
        ] {
            let message = refusal.message();
            assert!(!message.is_empty() && !message.contains('\u{2014}'));
            assert!(!message.contains("fee"), "{message}");
        }
    }

    #[test]
    fn new_commitment_is_pending_with_the_signup_deadline_and_advance_frozen() {
        let out = new_commitment(7, at("2026-10-01 10:00"), at("2026-10-31 10:00"), 30_000);
        let c = &out.commitment;
        assert_eq!(c.status, Status::Pending);
        assert_eq!(c.signup_advance_cents, 30_000);
        assert_eq!((c.terms, c.advance_item_id.as_deref()), (None, None));
        assert_eq!(
            out.events[0].detail,
            json!({ "signup_advance_cents": 30_000, "deadline_at": "2026-10-31T10:00:00Z" })
        );
    }

    // The charge date and the prepaid month

    #[test]
    fn the_advance_goes_on_the_trial_end_when_it_is_seven_days_away_else_the_next_period() {
        let trial_end = at("2027-01-28 15:00");
        let terms =
            advance_terms(30_000, Some(trial_end), trial_end, at("2026-11-01 12:00")).unwrap();
        assert_eq!(terms.advance_cents, 30_000);
        assert_eq!(terms.charge_at, trial_end);
        assert_eq!(terms.prepaid_from, at("2027-02-28 15:00"));
        assert_eq!(terms.prepaid_until, at("2027-03-28 15:00"));
        // Exactly 7 days before still takes the trial end.
        let edge = advance_terms(30_000, None, trial_end, at("2027-01-21 15:00")).unwrap();
        assert_eq!(edge.charge_at, trial_end);
        // Less than 7 days: the next period's charge.
        let late =
            advance_terms(30_000, Some(trial_end), trial_end, at("2027-01-21 15:01")).unwrap();
        assert_eq!(late.charge_at, at("2027-02-28 15:00"));
        assert_eq!(late.prepaid_from, at("2027-03-28 15:00"));
        assert_eq!(late.prepaid_until, at("2027-04-28 15:00"));
    }

    #[test]
    fn periods_follow_the_anchor_day_clamped_to_the_month_like_stripe() {
        // Anchor Jan 31: Feb 28, Mar 31, Apr 30.
        let jan31 = at("2027-01-31 15:00");
        let terms = advance_terms(10_000, Some(jan31), jan31, at("2026-12-01 12:00")).unwrap();
        assert_eq!(terms.charge_at, jan31);
        assert_eq!(terms.prepaid_from, at("2027-02-28 15:00"));
        assert_eq!(terms.prepaid_until, at("2027-03-31 15:00"));
        assert_eq!(
            period_after(jan31, at("2027-03-31 15:00")),
            Some(at("2027-04-30 15:00"))
        );
        // Anchor Feb 28: Mar 28, Apr 28.
        let feb28 = at("2027-02-28 15:00");
        let terms = advance_terms(10_000, Some(feb28), feb28, at("2027-01-10 12:00")).unwrap();
        assert_eq!(terms.prepaid_from, at("2027-03-28 15:00"));
        assert_eq!(terms.prepaid_until, at("2027-04-28 15:00"));
    }

    // The notice

    #[test]
    fn the_notice_states_the_terms_fixed_before_it_is_sent() {
        for c in [base(), booked("2026-11-03 15:00")] {
            let sent = noticed(&c);
            assert_eq!(sent.status, Status::NoticeSent);
            assert_eq!(sent.notice_sent_at, Some(at("2026-10-31 12:00")));
            assert_eq!(sent.terms, Some(terms()));
            assert_eq!(sent.booking, c.booking);
        }
        let out = notice_sent(
            &notice_terms(
                &cycle_started(&base(), "2026-10-31 11:00").commitment,
                terms(),
            )
            .commitment,
            at("2026-10-31 12:00"),
        );
        assert_eq!(kinds(&out), [EventKind::NoticeSent]);
        let detail = &out.events[0].detail;
        assert_eq!(detail["advance_cents"], 30_000);
        assert_eq!(detail["charge_at"], "2027-01-01T10:00:00Z");
        assert_eq!(detail["prepaid_until"], "2027-03-01T10:00:00Z");
        // No terms (or no cycle): no notice.
        assert_eq!(notice_terms(&base(), terms()), unchanged(&base()));
        assert_eq!(
            notice_sent(&base(), at("2026-10-31 12:00")),
            unchanged(&base())
        );
        let again = noticed(&base());
        assert_eq!(
            notice_sent(&again, at("2026-11-01 12:00")),
            unchanged(&again)
        );
    }

    #[test]
    fn the_item_falls_due_only_in_the_last_three_days_before_the_charge() {
        let c = noticed(&base());
        for now in ["2026-11-07 12:00", "2026-12-29 09:59"] {
            let day = run(&c, &facts(), now);
            assert_eq!(day.outcome, unchanged(&c), "at {now}");
            assert!(day.due.is_empty(), "at {now}");
        }
        for now in ["2026-12-29 10:00", "2027-01-01 09:59", "2027-01-02 10:00"] {
            assert_eq!(due_at(&c, now), [Due::AddItem], "at {now}");
        }
    }

    #[test]
    fn no_item_while_held_or_while_a_call_booked_to_end_by_the_charge_counts() {
        let held = Commitment {
            review_hold_at: Some(at("2026-11-02 10:00")),
            ..noticed(&base())
        };
        assert!(due_at(&held, "2026-12-30 10:00").is_empty());
        let call_first = book(&noticed(&base()), call("2026-12-31 15:00"));
        assert!(due_at(&call_first, "2026-12-30 10:00").is_empty());
        let call_after = book(&noticed(&base()), call("2027-01-01 10:00"));
        assert_eq!(due_at(&call_after, "2026-12-30 10:00"), [Due::AddItem]);
    }

    #[test]
    fn charging_off_or_no_consent_at_the_item_window_skips_the_advance() {
        for (case, reason) in [
            (
                DailyFacts {
                    charging_enabled: false,
                    ..facts()
                },
                "charging_off",
            ),
            (
                DailyFacts {
                    consent_on_file: false,
                    ..facts()
                },
                "no_consent",
            ),
        ] {
            let day = run(&noticed(&base()), &case, "2026-12-30 10:00");
            assert_eq!(day.outcome.commitment.status, Status::AdvanceSkipped);
            assert_eq!(kinds(&day.outcome), [EventKind::AdvanceSkipped]);
            assert_eq!(day.outcome.events[0].detail["reason"], reason);
            assert!(day.due.is_empty());
        }
    }

    #[test]
    fn no_running_subscription_at_the_deadline_voids_it_and_before_changes_nothing() {
        let none = DailyFacts {
            subscription_live: false,
            ..facts()
        };
        assert_eq!(
            run(&base(), &none, "2026-10-22 10:00").due,
            [Due::Reminder(ReminderStep::Day21)]
        );
        for c in [base(), noticed(&base()), scheduled()] {
            let day = run(&c, &none, "2026-11-02 10:00");
            assert_eq!(day.outcome.commitment.status, Status::Void);
            assert_eq!(day.outcome.events[0].detail["reason"], "no_subscription");
            let open = day.outcome.commitment.advance_item_id.is_some();
            assert_eq!(day.due.contains(&Due::SettleItem), open);
            assert!(!day.due.contains(&Due::Notice));
        }
    }

    #[test]
    fn subscription_end_voids_every_live_state() {
        let ended = DailyFacts {
            subscription_ended: true,
            ..facts()
        };
        for c in [
            base(),
            booked("2026-10-20 15:00"),
            with_status(Status::Suspended),
            noticed(&base()),
        ] {
            let day = run(&c, &ended, "2026-11-02 10:00");
            assert_eq!(day.outcome.commitment.status, Status::Void);
            assert_eq!(kinds(&day.outcome), [EventKind::Voided]);
            assert!(day.due.is_empty());
        }
    }

    // The item

    #[test]
    fn the_item_is_recorded_once_and_only_from_the_notice() {
        let out = advance_scheduled(&noticed(&base()), "ii_1", None, at("2026-12-29 10:00"));
        let c = &out.commitment;
        assert_eq!(c.status, Status::AdvanceScheduled);
        assert_eq!(c.advance_item_id.as_deref(), Some("ii_1"));
        assert_eq!(kinds(&out), [EventKind::AdvanceScheduled]);
        assert_eq!(out.events[0].detail["item_id"], "ii_1");
        let now = at("2026-12-29 11:00");
        assert_eq!(advance_scheduled(c, "ii_2", None, now), unchanged(c));
        for status in [Status::Pending, Status::Suspended, Status::Completed] {
            let other = with_status(status);
            assert_eq!(
                advance_scheduled(&other, "ii_2", None, now),
                unchanged(&other)
            );
        }
    }

    #[test]
    fn a_scheduled_advance_is_settled_every_run_and_otherwise_left_alone() {
        let c = scheduled();
        let day = run(&c, &facts(), "2026-12-30 10:00");
        assert_eq!(day.outcome, unchanged(&c));
        assert_eq!(day.due, [Due::SettleItem]);
        let invoiced = item_invoiced(&c, "in_1", at("2027-01-01 11:00")).commitment;
        assert!(is_invoiced(&invoiced));
        let busy = DailyFacts {
            subscription_ended: true,
            ..requested("2027-01-01 12:00")
        };
        let day = run(&invoiced, &busy, "2027-01-02 10:00");
        assert_eq!(day.outcome, unchanged(&invoiced));
        assert_eq!(day.due, [Due::SettleItem]);
        for action in ACTIONS {
            assert_eq!(
                superuser_action(&invoiced, action, ACTOR, at("2027-01-02 10:00")),
                Err(Refusal::AlreadyInvoiced)
            );
        }
    }

    #[test]
    fn completion_cancellation_waiver_or_hold_while_pending_leave_the_item_to_delete() {
        // A call that ends before the charge still counts.
        let call = book(&scheduled(), call("2026-12-31 15:00"));
        let after_call = run(&call, &facts(), "2027-01-02 16:00");
        assert_eq!(after_call.outcome.commitment.status, Status::Completed);
        assert_eq!(after_call.due, [Due::SettleItem]);

        let cancelled = run(
            &scheduled(),
            &requested("2026-12-30 09:00"),
            "2026-12-30 10:00",
        );
        assert_eq!(cancelled.outcome.commitment.status, Status::Suspended);
        assert_eq!(cancelled.due, [Due::SettleItem]);

        let now = at("2026-12-30 10:00");
        for (action, status) in [
            (Action::Waive, Status::Waived),
            (Action::Complete, Status::Completed),
            (Action::Hold, Status::NoticeSent),
        ] {
            let out = superuser_action(&scheduled(), action, ACTOR, now).unwrap();
            assert_eq!(out.commitment.status, status);
            assert!(item_open(&out.commitment));
            assert_eq!(
                daily(&out.commitment, &facts(), now).due[0],
                Due::SettleItem
            );
        }
    }

    #[test]
    fn a_call_booked_to_end_by_the_charge_takes_the_scheduled_item_back_at_once() {
        let now = at("2026-12-30 10:00");
        let out = record_booking(&scheduled(), call("2026-12-31 15:00"), now).unwrap();
        assert_eq!(out.commitment.status, Status::NoticeSent);
        assert_eq!(
            kinds(&out),
            [EventKind::Booked, EventKind::AdvanceWithdrawn]
        );
        assert_eq!(out.events[1].detail["reason"], "call_booked");
        assert_eq!(out.events[1].detail["item_id"], "ii_1");
        assert!(item_open(&out.commitment));
        assert!(call_avoids_advance(&out.commitment));
        assert_eq!(
            run(&out.commitment, &facts(), "2026-12-30 11:00").due,
            [Due::SettleItem]
        );
        // Nothing may put an item back while that call is booked.
        let ok = ChargeFacts {
            next_charge_at: Some(at("2027-01-01 10:00")),
            ..charge_facts()
        };
        assert_eq!(
            withdrawal(&out.commitment, &ok),
            Some(Withdrawal::CallBooked)
        );
        let noticed_and_booked = book(&noticed(&base()), call("2026-12-31 15:00"));
        assert_eq!(
            withdrawal(&noticed_and_booked, &ok),
            Some(Withdrawal::CallBooked)
        );
        let waits = advance_withdrawn(&noticed_and_booked, Withdrawal::CallBooked, now);
        assert_eq!(waits.commitment, noticed_and_booked);

        // A call ending after the charge leaves the advance scheduled.
        let late = record_booking(&scheduled(), call("2027-01-01 10:00"), now).unwrap();
        assert_eq!(late.commitment.status, Status::AdvanceScheduled);
        assert_eq!(kinds(&late), [EventKind::Booked]);
        assert_eq!(withdrawal(&late.commitment, &ok), None);

        // A booking the daily run finds does the same.
        let found = BookingCheck::Found(call("2026-12-31 15:00"));
        let day = run(&scheduled(), &with(found), "2026-12-30 10:00");
        assert_eq!(day.outcome.commitment.status, Status::NoticeSent);
        assert_eq!(
            kinds(&day.outcome),
            [EventKind::Booked, EventKind::AdvanceWithdrawn]
        );
        assert_eq!(day.due, [Due::SettleItem]);
    }

    #[test]
    fn an_advance_credited_off_after_the_subscription_ended_voids_and_forgets_the_item() {
        let c = item_invoiced(&scheduled(), "in_1", at("2027-01-01 11:00")).commitment;
        let out = item_credited_off(&c, "in_1", "cn_1", at("2027-01-20 10:00"));
        assert_eq!(out.commitment.status, Status::Void);
        assert!(!item_open(&out.commitment));
        assert_eq!(kinds(&out), [EventKind::Voided]);
        assert_eq!(out.events[0].detail["reason"], "subscription_ended");
        assert_eq!(out.events[0].detail["credit_note_id"], "cn_1");
        assert_eq!(out.events[0].detail["item_id"], "ii_1");
        assert_eq!(
            item_credited_off(&out.commitment, "in_1", "cn_1", at("2027-01-20 10:01")).events,
            []
        );
        let waived = Commitment {
            status: Status::Waived,
            ..c
        };
        assert_eq!(
            item_credited_off(&waived, "in_1", "cn_1", at("2027-01-20 10:00"))
                .commitment
                .status,
            Status::Waived
        );
    }

    #[test]
    fn a_deleted_item_is_forgotten_and_a_vanished_one_goes_back_to_the_notice() {
        let gone = item_withdrawn(&scheduled(), "item_missing", at("2026-12-30 10:00"));
        assert_eq!(gone.commitment.status, Status::NoticeSent);
        assert_eq!(gone.commitment.advance_item_id, None);
        assert_eq!(kinds(&gone), [EventKind::AdvanceWithdrawn]);
        let waived = superuser_action(&scheduled(), Action::Waive, ACTOR, at("2026-12-30 10:00"))
            .unwrap()
            .commitment;
        let deleted = item_withdrawn(&waived, "not_wanted", at("2026-12-30 10:01")).commitment;
        assert_eq!(deleted.status, Status::Waived);
        assert!(!item_open(&deleted));
        assert!(
            daily(&deleted, &facts(), at("2026-12-31 10:00"))
                .due
                .is_empty()
        );
    }

    #[test]
    fn a_paid_invoice_credits_once_and_a_void_one_voids() {
        let c = item_invoiced(&scheduled(), "in_1", at("2027-01-01 11:00")).commitment;
        let paid = credited(&c, "in_1", "cbtxn_1", at("2027-01-01 12:00"));
        assert_eq!(paid.commitment.status, Status::AdvancePaid);
        assert_eq!(
            paid.commitment.advance_credit_id.as_deref(),
            Some("cbtxn_1")
        );
        assert_eq!(kinds(&paid), [EventKind::AdvancePaid]);
        assert!(!item_open(&paid.commitment));
        let again = credited(&paid.commitment, "in_1", "cbtxn_2", at("2027-01-02 12:00"));
        assert_eq!(again, unchanged(&paid.commitment));

        let ended = invoice_ended(&c, "in_1", "void", at("2027-01-05 12:00"));
        assert_eq!(ended.commitment.status, Status::Void);
        assert_eq!(ended.commitment.advance_item_id, None);
        assert_eq!(ended.events[0].detail["reason"], "invoice_void");
        assert_eq!(ended.commitment.advance_credit_id, None);

        // Completed after Stripe swept the item: the credit keeps it completed.
        let done = Commitment {
            status: Status::Completed,
            ..c
        };
        let credit = credited(&done, "in_1", "cbtxn_1", at("2027-01-01 12:00")).commitment;
        assert_eq!(credit.status, Status::Completed);
        assert_eq!(credit.advance_credit_id.as_deref(), Some("cbtxn_1"));
    }

    #[test]
    fn an_untracked_item_found_on_a_paid_invoice_is_recorded() {
        let found = item_recorded(
            &noticed(&base()),
            "ii_9",
            Some("in_9"),
            at("2027-01-01 12:00"),
        );
        assert_eq!(found.commitment.status, Status::AdvanceScheduled);
        assert_eq!(found.commitment.stripe_invoice_id.as_deref(), Some("in_9"));
        let dropped = item_recorded(&base(), "ii_9", Some("in_9"), at("2027-01-01 12:00"));
        assert_eq!(dropped.commitment.status, Status::Pending);
        assert!(item_open(&dropped.commitment));
        let paid = credited(
            &dropped.commitment,
            "in_9",
            "cbtxn_9",
            at("2027-01-01 12:01"),
        );
        assert_eq!(paid.commitment.status, Status::AdvancePaid);
    }

    #[test]
    fn the_advance_is_withdrawn_for_an_end_a_request_a_hold_charging_off_or_a_moved_charge() {
        let c = scheduled();
        let ok = ChargeFacts {
            next_charge_at: Some(at("2027-01-01 10:00")),
            ..charge_facts()
        };
        assert_eq!(withdrawal(&c, &ok), None);
        let cases = [
            (
                ChargeFacts {
                    subscription_live: false,
                    ..ok
                },
                Withdrawal::SubscriptionEnded,
            ),
            (
                ChargeFacts {
                    cancellation_requested_at: Some(at("2026-12-30 09:00")),
                    ..ok
                },
                Withdrawal::CancellationRequested(at("2026-12-30 09:00")),
            ),
            (
                ChargeFacts {
                    cancel_scheduled: true,
                    ..ok
                },
                Withdrawal::CancelScheduled,
            ),
            (
                ChargeFacts {
                    charging_enabled: false,
                    ..ok
                },
                Withdrawal::ChargingOff,
            ),
            (
                ChargeFacts {
                    consent_on_file: false,
                    ..ok
                },
                Withdrawal::NoConsent,
            ),
            (
                ChargeFacts {
                    next_charge_at: Some(at("2027-02-01 10:00")),
                    ..ok
                },
                Withdrawal::ChargeMoved,
            ),
        ];
        for (facts, reason) in cases {
            assert_eq!(withdrawal(&c, &facts), Some(reason));
        }
        // A request on or after the charge does not take the advance back.
        let late = ChargeFacts {
            cancellation_requested_at: Some(at("2027-01-01 10:00")),
            ..ok
        };
        assert_eq!(withdrawal(&c, &late), None);
        let held = Commitment {
            review_hold_at: Some(at("2026-12-30 09:00")),
            ..c.clone()
        };
        assert_eq!(withdrawal(&held, &ok), Some(Withdrawal::Held));

        let now = at("2026-12-30 10:00");
        let status = |reason| advance_withdrawn(&c, reason, now).commitment.status;
        assert_eq!(status(Withdrawal::SubscriptionEnded), Status::Void);
        assert_eq!(
            status(Withdrawal::CancellationRequested(at("2026-12-30 09:00"))),
            Status::Suspended
        );
        assert_eq!(status(Withdrawal::CancelScheduled), Status::NoticeSent);
        assert_eq!(status(Withdrawal::Held), Status::NoticeSent);
        assert_eq!(status(Withdrawal::ChargingOff), Status::AdvanceSkipped);
        assert_eq!(status(Withdrawal::NoConsent), Status::AdvanceSkipped);
        let moved = advance_withdrawn(&c, Withdrawal::ChargeMoved, now).commitment;
        assert_eq!((moved.status, moved.terms), (Status::Pending, None));
        assert!(item_open(&moved));
    }

    #[test]
    fn a_missed_charge_drops_the_notice_and_a_new_one_falls_due() {
        let dropped = drop_notice(&noticed(&base()), "charge_missed", at("2027-01-02 10:00"));
        let c = &dropped.commitment;
        assert_eq!(
            (c.status, c.terms, c.notice_sent_at),
            (Status::Pending, None, None)
        );
        assert_eq!(kinds(&dropped), [EventKind::NoticeWithdrawn]);
        let day = run(c, &facts(), "2027-01-02 10:00");
        assert_eq!(day.due, [Due::Notice]);
    }

    // Cancellation and its withdrawal

    #[test]
    fn a_request_during_the_notice_suspends_and_keeps_the_terms() {
        let day = run(
            &noticed(&base()),
            &requested("2026-11-03 09:00"),
            "2026-11-03 10:00",
        );
        let out = &day.outcome;
        assert_eq!(out.commitment.status, Status::Suspended);
        assert_eq!(out.commitment.terms, Some(terms()));
        assert_eq!(out.events[0].detail["notice_held"], true);
        assert!(day.due.is_empty());
        // On or after the charge date a request does not suspend.
        let late = run(
            &noticed(&base()),
            &requested("2027-01-01 10:00"),
            "2027-01-01 11:00",
        );
        assert_eq!(late.outcome.commitment.status, Status::NoticeSent);
    }

    #[test]
    fn a_withdrawal_keeps_a_charge_seven_days_away_else_a_new_notice_comes() {
        let suspended = run(
            &noticed(&base()),
            &requested("2026-11-03 09:00"),
            "2026-11-03 10:00",
        )
        .outcome
        .commitment;
        assert_eq!(
            resumption(&suspended, at("2026-12-25 10:00")),
            Resumption::KeepsCharge(at("2027-01-01 10:00"))
        );
        let kept = run(&suspended, &facts(), "2026-12-25 10:00");
        assert_eq!(kept.outcome.commitment.status, Status::NoticeSent);
        assert_eq!(kept.outcome.events[0].detail["resumes_at"], "notice_sent");
        assert_eq!(
            kept.outcome.events[0].detail["charge_at"],
            "2027-01-01T10:00:00Z"
        );

        assert_eq!(
            resumption(&suspended, at("2026-12-25 10:01")),
            Resumption::NewNotice
        );
        let renotice = run(&suspended, &facts(), "2026-12-25 10:01");
        let c = &renotice.outcome.commitment;
        assert_eq!(
            (c.status, c.terms, c.notice_sent_at),
            (Status::Pending, None, None)
        );
        assert_eq!(renotice.outcome.events[0].detail["resumes_at"], "notice");
        assert_eq!(renotice.due, [Due::Notice]);

        // Withdrawn before the deadline: the countdown.
        let early = run(&base(), &requested("2026-10-20 09:00"), "2026-10-20 10:00")
            .outcome
            .commitment;
        assert_eq!(
            resumption(&early, at("2026-10-25 10:00")),
            Resumption::Countdown
        );
    }

    #[test]
    fn a_request_and_withdrawal_between_two_runs_inside_the_notice_week_need_a_new_notice() {
        let c = noticed(&base());
        let across = DailyFacts {
            withdrawn_cancellations: vec![(at("2026-12-26 09:00"), at("2026-12-27 10:00"))],
            ..facts()
        };
        let day = run(&c, &across, "2026-12-27 14:00");
        assert_eq!(day.outcome.commitment.status, Status::Pending);
        assert_eq!(kinds(&day.outcome), [EventKind::Resumed]);
        assert_eq!(day.due, [Due::Notice]);
        let early = DailyFacts {
            withdrawn_cancellations: vec![(at("2026-12-20 09:00"), at("2026-12-21 10:00"))],
            ..facts()
        };
        assert_eq!(run(&c, &early, "2026-12-27 14:00").outcome, unchanged(&c));
    }

    #[test]
    fn a_cancellation_after_the_paid_advance_ends_with_the_prepaid_month() {
        let paid = credited(
            &item_invoiced(&scheduled(), "in_1", at("2027-01-01 11:00")).commitment,
            "in_1",
            "cbtxn_1",
            at("2027-01-01 12:00"),
        )
        .commitment;
        assert_eq!(
            prepaid_cancel_at(&paid, at("2027-01-15 10:00")),
            Some(at("2027-03-01 10:00"))
        );
        assert_eq!(prepaid_cancel_at(&paid, at("2027-02-01 10:00")), None);
        assert_eq!(
            prepaid_cancel_at(&scheduled(), at("2027-01-15 10:00")),
            None
        );
    }

    // Review hold

    #[test]
    fn hold_and_release_with_a_new_notice_when_the_charge_is_under_seven_days_away() {
        let now = at("2026-12-20 10:00");
        let held = superuser_action(&noticed(&base()), Action::Hold, ACTOR, now).unwrap();
        assert_eq!(held.commitment.review_hold_at, Some(now));
        assert_eq!(held.commitment.review_hold_by, Some(ACTOR));
        assert_eq!(kinds(&held), [EventKind::ReviewHeld]);
        assert_eq!(
            superuser_action(&held.commitment, Action::Hold, ACTOR, now),
            Err(Refusal::AlreadyHeld)
        );
        assert_eq!(
            superuser_action(&noticed(&base()), Action::Release, ACTOR, now),
            Err(Refusal::NotHeld)
        );
        // A held company gets no notice at the deadline either.
        let held_early = superuser_action(&base(), Action::Hold, ACTOR, at("2026-10-20 10:00"))
            .unwrap()
            .commitment;
        assert!(due_at(&held_early, "2026-11-01 10:00").is_empty());

        let kept = superuser_action(
            &held.commitment,
            Action::Release,
            ACTOR,
            at("2026-12-25 10:00"),
        )
        .unwrap();
        assert_eq!(kept.commitment.status, Status::NoticeSent);
        assert_eq!(kept.commitment.terms, Some(terms()));
        assert_eq!(kept.events[0].detail["new_notice"], false);
        let late = superuser_action(
            &held.commitment,
            Action::Release,
            ACTOR,
            at("2026-12-25 10:01"),
        )
        .unwrap();
        assert_eq!(
            (late.commitment.status, late.commitment.terms),
            (Status::Pending, None)
        );
        assert_eq!(late.commitment.review_hold_at, None);
        assert_eq!(late.events[0].detail["new_notice"], true);
        assert_eq!(due_at(&late.commitment, "2026-12-25 11:00"), [Due::Notice]);
    }

    #[test]
    fn the_allowed_actions_and_the_extended_deadline_come_from_the_rules() {
        let now = at("2026-10-20 15:30");
        let c = booked("2026-10-20 15:00");
        assert_eq!(
            allowed_actions(&c, now),
            [
                Action::Extend,
                Action::Waive,
                Action::Complete,
                Action::NoShow,
                Action::Hold
            ]
        );
        assert_eq!(extension_deadline(&c, now), Some(at("2026-11-14 11:00")));
        let used = superuser_action(&c, Action::Extend, ACTOR, now)
            .unwrap()
            .commitment;
        assert_eq!(extension_deadline(&used, now), None);
        assert!(!allowed_actions(&used, now).contains(&Action::Extend));
        let held = superuser_action(&base(), Action::Hold, ACTOR, now)
            .unwrap()
            .commitment;
        assert_eq!(
            allowed_actions(&held, now),
            [
                Action::Extend,
                Action::Waive,
                Action::Complete,
                Action::Release
            ]
        );
        for status in SETTLED {
            assert!(allowed_actions(&with_status(status), now).is_empty());
        }
    }

    #[test]
    fn booking_refused_on_settled_and_invoiced_commitments() {
        let now = at("2026-11-10 10:00");
        for status in SETTLED {
            let refused = record_booking(&with_status(status), call("2026-11-20 15:00"), now);
            assert_eq!(refused, Err(Refusal::Final));
        }
        let invoiced = item_invoiced(&scheduled(), "in_1", now).commitment;
        assert_eq!(
            record_booking(&invoiced, call("2026-11-20 15:00"), now),
            Err(Refusal::AlreadyInvoiced)
        );
    }

    #[test]
    fn extension_in_the_notice_period_returns_to_pending_and_withdraws_the_notice() {
        let out = superuser_action(
            &noticed(&base()),
            Action::Extend,
            ACTOR,
            at("2026-11-02 10:00"),
        )
        .unwrap();
        let extended = &out.commitment;
        assert_eq!(extended.status, Status::Pending);
        assert_eq!(extended.deadline_at, at("2026-11-14 11:00"));
        assert_eq!((extended.notice_sent_at, extended.terms), (None, None));
        assert_eq!(out.events[0].detail["notice_withdrawn"], true);
    }

    #[test]
    fn host_cancellation_after_the_notice_withdraws_it_when_the_deadline_is_ahead_again() {
        let recorded = Booking {
            booked_at: at("2026-10-25 10:00"),
            ..call("2026-11-03 15:00")
        };
        let c = noticed(&book(&base(), recorded));
        let cancelled = with(BookingCheck::Cancelled {
            by_host: true,
            at: at("2026-11-01 12:00"),
        });
        let day = run(&c, &cancelled, "2026-11-01 13:00");
        let out = &day.outcome;
        assert_eq!(out.commitment.status, Status::Pending);
        assert_eq!(out.commitment.deadline_at, at("2026-11-08 11:00"));
        assert_eq!(
            (out.commitment.notice_sent_at, out.commitment.terms),
            (None, None)
        );
        assert_eq!(out.events[0].detail["notice_withdrawn"], true);
        assert_eq!(day.due, [Due::Reminder(ReminderStep::Day27)]);
    }

    // Tolling counts the buyer's days

    #[test]
    fn an_evening_run_tolls_the_local_day_not_the_utc_one() {
        // 2026-10-11 02:00 UTC is 22:00 EDT on 10-10.
        let no_slots = DailyFacts {
            availability: Availability::NoSlots,
            ..facts()
        };
        let day = run(&base(), &no_slots, "2026-10-11 02:00");
        let c = &day.outcome.commitment;
        assert_eq!(c.availability_checked_on, Some(date("2026-10-10")));
        assert_eq!(day.outcome.events[0].detail["day"], "2026-10-10");
        // The next morning (local 10-11) is a new day.
        let next = run(c, &no_slots, "2026-10-11 12:00").outcome.commitment;
        assert_eq!(next.availability_checked_on, Some(date("2026-10-11")));
        assert_eq!(next.tolled_days, 2);
        assert_eq!(local_day(at("2026-10-11 03:59")), date("2026-10-10"));
        assert_eq!(
            start_of_local_day(date("2026-10-11")),
            at("2026-10-11 04:00")
        );
        assert_eq!(secs("2026-10-11 04:00:00"), at("2026-10-11 04:00"));
    }

    #[test]
    fn booking_moves_pending_to_booked() {
        let out =
            record_booking(&base(), call("2026-10-20 15:00"), at("2026-10-02 10:00")).unwrap();
        assert_eq!(out.commitment.status, Status::Booked);
        assert_eq!(out.commitment.booking, Some(call("2026-10-20 15:00")));
        assert_eq!(kinds(&out), [EventKind::Booked]);
        assert_eq!(out.events[0].from, Some(Status::Pending));
        assert_eq!(out.events[0].detail["source"], "widget");
        assert_eq!(
            out.events[0].detail["call_start_at"],
            "2026-10-20T15:00:00Z"
        );
    }

    #[test]
    fn same_booking_again_changes_nothing() {
        let c = booked("2026-10-20 15:00");
        let again = Booking {
            booked_at: at("2026-10-03 09:00"),
            ..call("2026-10-20 15:00")
        };
        let out = record_booking(&c, again, at("2026-10-03 09:00")).unwrap();
        assert_eq!(out, unchanged(&c));
    }

    #[test]
    fn new_booking_replaces_the_stored_one_as_a_reschedule() {
        let c = booked("2026-10-20 15:00");
        let out = record_booking(&c, call("2026-10-23 15:00"), at("2026-10-03 09:00")).unwrap();
        assert_eq!(out.commitment.status, Status::Booked);
        assert_eq!(out.commitment.booking, Some(call("2026-10-23 15:00")));
        assert_eq!(out.commitment.deadline_at, c.deadline_at);
        assert_eq!(kinds(&out), [EventKind::Rescheduled]);
        assert_eq!(out.events[0].detail["old_start_at"], "2026-10-20T15:00:00Z");
        assert_eq!(out.events[0].detail["new_start_at"], "2026-10-23T15:00:00Z");
    }

    #[test]
    fn the_missed_call_or_an_older_one_is_refused_and_a_later_one_books() {
        let c = missed();
        let now = at("2026-10-20 16:00");
        for start in ["2026-10-20 15:00", "2026-10-19 15:00"] {
            assert_eq!(
                record_booking(&c, call(start), now),
                Err(Refusal::MissedCall)
            );
        }
        let later = record_booking(&c, call("2026-10-24 15:00"), now).unwrap();
        assert_eq!(later.commitment.status, Status::Booked);
        assert_eq!(later.commitment.booking, Some(call("2026-10-24 15:00")));
        assert_eq!(kinds(&later), [EventKind::Booked]);
    }

    #[test]
    fn a_call_starting_after_the_missed_one_counts_even_before_the_no_show_mark() {
        // Missed call 15:00, marked 15:30: a call at 15:15 is a new one.
        let c = missed();
        let between = call("2026-10-20 15:15");
        let booked = record_booking(&c, between.clone(), at("2026-10-20 16:00")).unwrap();
        assert_eq!(booked.commitment.booking, Some(between.clone()));
        let found = run(&c, &with(BookingCheck::Found(between)), "2026-10-22 16:15");
        assert_eq!(found.outcome.commitment.status, Status::Completed);
        assert_eq!(
            kinds(&found.outcome),
            [EventKind::Booked, EventKind::Completed]
        );
    }

    #[test]
    fn found_booking_is_stored_and_books_a_pending_company() {
        let found = with(BookingCheck::Found(call("2026-10-20 15:00")));
        let day = run(&base(), &found, "2026-10-05 10:00");
        let c = &day.outcome.commitment;
        assert_eq!(c.status, Status::Booked);
        assert_eq!(c.booking, Some(call("2026-10-20 15:00")));
        assert_eq!(kinds(&day.outcome), [EventKind::Booked]);
        assert_eq!(day.outcome.events[0].detail["source"], "found");
    }

    #[test]
    fn the_missed_call_found_again_changes_nothing_and_never_completes() {
        let c = missed();
        let found = with(BookingCheck::Found(call("2026-10-20 15:00")));
        for now in ["2026-10-21 10:00", "2026-10-23 10:00", "2026-11-05 10:00"] {
            let day = run(&c, &found, now);
            assert_eq!(day.outcome.commitment.booking, None, "at {now}");
            assert!(day.outcome.events.is_empty(), "at {now}");
            assert_ne!(day.outcome.commitment.status, Status::Completed);
        }
        let later = with(BookingCheck::Found(call("2026-10-24 15:00")));
        let day = run(&c, &later, "2026-10-21 10:00");
        assert_eq!(day.outcome.commitment.status, Status::Booked);
        assert_eq!(kinds(&day.outcome), [EventKind::Booked]);
    }

    #[test]
    fn found_booking_while_suspended_keeps_the_suspension() {
        let c = Commitment {
            suspended_at: Some(at("2026-10-04 10:00")),
            ..with_status(Status::Suspended)
        };
        let found = DailyFacts {
            cancellation_requested_at: Some(at("2026-10-04 09:00")),
            ..with(BookingCheck::Found(call("2026-10-20 15:00")))
        };
        let day = run(&c, &found, "2026-10-05 10:00");
        assert_eq!(day.outcome.commitment.status, Status::Suspended);
        assert!(day.outcome.commitment.booking.is_some());
        assert_eq!(kinds(&day.outcome), [EventKind::Booked]);
        assert!(day.due.is_empty());
    }

    #[test]
    fn calendly_no_show_within_48_hours_unbooks() {
        let c = booked("2026-10-20 15:00");
        let mark = at("2026-10-22 16:00");
        let check = with(BookingCheck::Active {
            no_show_at: Some(mark),
        });
        let day = run(&c, &check, "2026-10-22 18:00");
        let out = &day.outcome;
        assert_eq!(out.commitment.status, Status::Pending);
        assert_eq!(out.commitment.booking, None);
        assert_eq!(out.commitment.no_show_at, Some(mark));
        assert_eq!(
            out.commitment.no_show_call_start_at,
            Some(at("2026-10-20 15:00"))
        );
        assert_eq!(kinds(out), [EventKind::NoShow]);
        assert_eq!(out.events[0].detail["source"], "calendly");
        assert_eq!(day.due, [Due::Reminder(ReminderStep::Day21)]);
    }

    #[test]
    fn calendly_no_show_after_48_hours_is_ignored_and_the_call_counts() {
        let c = booked("2026-10-20 15:00");
        let check = with(BookingCheck::Active {
            no_show_at: Some(at("2026-10-22 16:01")),
        });
        let day = run(&c, &check, "2026-10-22 18:00");
        assert_eq!(day.outcome.commitment.status, Status::Completed);
        assert_eq!(day.outcome.commitment.no_show_at, None);
        assert_eq!(kinds(&day.outcome), [EventKind::Completed]);
        assert!(day.due.is_empty());
    }

    #[test]
    fn customer_reschedule_keeps_the_deadline() {
        let c = booked("2026-10-20 15:00");
        let moved = with(BookingCheck::Moved(call("2026-10-27 15:00")));
        let day = run(&c, &moved, "2026-10-12 10:00");
        let out = &day.outcome;
        assert_eq!(out.commitment.booking, Some(call("2026-10-27 15:00")));
        assert_eq!(out.commitment.deadline_at, c.deadline_at);
        assert_eq!(kinds(out), [EventKind::Rescheduled]);
        assert_eq!(out.events[0].detail["by"], "customer");
    }

    #[test]
    fn host_reschedule_extends_by_the_days_it_moved_the_call() {
        let c = booked("2026-10-20 15:00");
        // Two days and one hour later: three days back.
        let moved = with(BookingCheck::MovedByHost(call("2026-10-22 16:00")));
        let day = run(&c, &moved, "2026-10-12 10:00");
        let out = &day.outcome;
        assert_eq!(out.commitment.booking, Some(call("2026-10-22 16:00")));
        assert_eq!(out.commitment.deadline_at, at("2026-11-03 11:00"));
        assert_eq!(out.commitment.original_deadline_at, at("2026-10-31 10:00"));
        assert_eq!(kinds(out), [EventKind::Rescheduled]);
        assert_eq!(out.events[0].deadline_at, at("2026-11-03 11:00"));
        assert_eq!(out.events[0].detail["by"], "host");
        assert_eq!(out.events[0].detail["days_added"], 3);
    }

    #[test]
    fn host_moving_a_call_earlier_still_adds_a_day() {
        let c = booked("2026-10-20 15:00");
        let moved = with(BookingCheck::MovedByHost(call("2026-10-19 15:00")));
        let day = run(&c, &moved, "2026-10-12 10:00");
        assert_eq!(day.outcome.commitment.deadline_at, at("2026-11-01 11:00"));
    }

    #[test]
    fn customer_cancellation_unbooks_without_extending() {
        let c = booked("2026-10-20 15:00");
        let cancelled = with(BookingCheck::Cancelled {
            by_host: false,
            at: at("2026-10-05 09:00"),
        });
        let day = run(&c, &cancelled, "2026-10-05 10:00");
        let out = &day.outcome;
        assert_eq!(out.commitment.status, Status::Pending);
        assert_eq!(out.commitment.booking, None);
        assert_eq!(out.commitment.deadline_at, c.deadline_at);
        assert_eq!(kinds(out), [EventKind::BookingCancelled]);
        assert_eq!(out.events[0].detail["by"], "customer");
    }

    #[test]
    fn host_cancellation_gives_back_the_days_waited() {
        let c = booked("2026-10-20 15:00");
        // Recorded 10-02 10:00, cancelled 10-05 09:00: 2 days 23 hours, 3 days back.
        let cancelled = with(BookingCheck::Cancelled {
            by_host: true,
            at: at("2026-10-05 09:00"),
        });
        let day = run(&c, &cancelled, "2026-10-05 10:00");
        let out = &day.outcome;
        assert_eq!(out.commitment.status, Status::Pending);
        assert_eq!(out.commitment.booking, None);
        assert_eq!(out.commitment.deadline_at, at("2026-11-03 11:00"));
        assert_eq!(out.events[0].detail["days_added"], 3);
        assert_eq!(out.events[0].detail["by"], "host");
    }

    #[test]
    fn unknown_or_quiet_calendly_changes_nothing() {
        let c = booked("2026-10-20 15:00");
        for check in [
            BookingCheck::Unknown,
            BookingCheck::Active { no_show_at: None },
            BookingCheck::None,
        ] {
            let day = run(&c, &with(check), "2026-10-12 10:00");
            assert_eq!(day.outcome, unchanged(&c));
        }
    }

    #[test]
    fn call_completes_exactly_48_hours_after_it_ends_not_a_minute_before() {
        let c = booked("2026-10-20 15:00");
        assert_eq!(run(&c, &facts(), "2026-10-22 15:59").outcome, unchanged(&c));
        let day = run(&c, &facts(), "2026-10-22 16:00");
        let done = &day.outcome.commitment;
        assert_eq!(done.status, Status::Completed);
        assert_eq!(done.completed_at, Some(at("2026-10-22 16:00")));
        assert_eq!(done.completed_by, None);
        assert_eq!(kinds(&day.outcome), [EventKind::Completed]);
        assert_eq!(day.outcome.events[0].detail["basis"], "call");
        assert_eq!(
            day.outcome.events[0].detail["call_end_at"],
            "2026-10-20T16:00:00Z"
        );
        assert!(day.due.is_empty());
    }

    #[test]
    fn call_ending_after_the_deadline_does_not_complete() {
        // Ends 11:00, after the 10:00 deadline.
        let c = booked("2026-10-31 10:00");
        let day = run(&c, &facts(), "2026-11-03 12:00");
        assert_eq!(day.outcome, cycle_started(&c, "2026-11-03 12:00"));
        assert_eq!(day.due, [Due::Notice]);
    }

    #[test]
    fn suspended_company_completes_its_call_whenever_it_ends() {
        let c = Commitment {
            status: Status::Suspended,
            booking: Some(call("2026-11-10 15:00")),
            suspended_at: Some(at("2026-10-20 10:00")),
            ..base()
        };
        let requested = DailyFacts {
            cancellation_requested_at: Some(at("2026-10-20 09:00")),
            ..facts()
        };
        let day = run(&c, &requested, "2026-11-12 16:00");
        assert_eq!(day.outcome.commitment.status, Status::Completed);
    }

    #[test]
    fn cancellation_requested_before_the_deadline_suspends() {
        let requested = DailyFacts {
            cancellation_requested_at: Some(at("2026-10-15 09:00")),
            ..facts()
        };
        let day = run(&base(), &requested, "2026-10-22 10:00");
        let out = &day.outcome;
        assert_eq!(out.commitment.status, Status::Suspended);
        assert_eq!(out.commitment.suspended_at, Some(at("2026-10-22 10:00")));
        assert_eq!(kinds(out), [EventKind::Suspended]);
        assert_eq!(out.events[0].detail["requested_at"], "2026-10-15T09:00:00Z");
        assert!(day.due.is_empty());
    }

    #[test]
    fn suspended_with_the_request_still_scheduled_does_nothing_more() {
        let c = Commitment {
            status: Status::Suspended,
            suspended_at: Some(at("2026-10-15 10:00")),
            ..base()
        };
        let request = DailyFacts {
            cancellation_requested_at: Some(at("2026-10-15 09:00")),
            availability: Availability::NoSlots,
            ..facts()
        };
        // No tolling, no reminder, no notice after the deadline.
        for now in ["2026-10-22 10:00", "2026-11-05 10:00"] {
            let day = run(&c, &request, now);
            assert_eq!(day.outcome, unchanged(&c));
            assert!(day.due.is_empty());
        }
    }

    #[test]
    fn withdrawal_before_the_deadline_resumes_and_runs_the_day() {
        let c = Commitment {
            status: Status::Suspended,
            suspended_at: Some(at("2026-10-15 10:00")),
            booking: Some(call("2026-10-25 15:00")),
            ..base()
        };
        let no_slots = DailyFacts {
            availability: Availability::NoSlots,
            ..facts()
        };
        let day = run(&c, &no_slots, "2026-10-22 10:00");
        let out = &day.outcome;
        assert_eq!(out.commitment.status, Status::Booked);
        assert_eq!(out.commitment.suspended_at, None);
        assert_eq!(kinds(out), [EventKind::Resumed, EventKind::Tolled]);
        assert_eq!(out.events[0].detail["resumes_at"], "countdown");
        assert_eq!(out.commitment.deadline_at, at("2026-11-01 11:00"));
        assert_eq!(day.due, [Due::Reminder(ReminderStep::Day21)]);
    }

    #[test]
    fn day_without_slots_tolls_one_day_once() {
        let no_slots = DailyFacts {
            availability: Availability::NoSlots,
            ..facts()
        };
        let first = run(&base(), &no_slots, "2026-10-10 06:00");
        let c = &first.outcome.commitment;
        assert_eq!(c.deadline_at, at("2026-11-01 11:00"));
        assert_eq!(c.tolled_days, 1);
        assert_eq!(c.availability_checked_on, Some(date("2026-10-10")));
        assert_eq!(kinds(&first.outcome), [EventKind::Tolled]);
        let tolled = &first.outcome.events[0];
        assert_eq!(tolled.deadline_at, at("2026-11-01 11:00"));
        assert_eq!(tolled.detail["reason"], "no_slots");
        assert_eq!(tolled.detail["old_deadline_at"], "2026-10-31T10:00:00Z");
        assert_eq!(tolled.detail["new_deadline_at"], "2026-11-01T11:00:00Z");

        assert_eq!(run(c, &no_slots, "2026-10-10 18:00").outcome, unchanged(c));

        let next = run(c, &no_slots, "2026-10-11 06:00").outcome.commitment;
        assert_eq!(next.deadline_at, at("2026-11-02 11:00"));
        assert_eq!(next.tolled_days, 2);
    }

    #[test]
    fn days_the_job_missed_or_found_without_slots_toll_and_slot_days_do_not() {
        let mut c = base();
        c.availability_checked_on = Some(date("2026-10-06"));
        let catch_up = DailyFacts {
            availability: Availability::Available,
            earlier_days: vec![
                (date("2026-10-06"), Availability::NoSlots),
                (date("2026-10-07"), Availability::Available),
                (date("2026-10-08"), Availability::NotChecked),
                (date("2026-10-09"), Availability::NoSlots),
            ],
            ..facts()
        };
        let day = run(&c, &catch_up, "2026-10-10 06:00");
        let after = &day.outcome.commitment;
        assert_eq!(after.tolled_days, 2);
        assert_eq!(after.deadline_at, at("2026-11-02 11:00"));
        assert_eq!(after.availability_checked_on, Some(date("2026-10-10")));
        let reasons: Vec<_> = day
            .outcome
            .events
            .iter()
            .map(|e| e.detail["reason"].clone())
            .collect();
        assert_eq!(reasons, ["not_checked", "no_slots"]);
        assert_eq!(
            run(after, &catch_up, "2026-10-10 18:00").outcome,
            unchanged(after)
        );
    }

    #[test]
    fn missing_booking_page_and_calendly_errors_toll_too() {
        for (availability, reason) in [
            (Availability::NotConfigured, "not_configured"),
            (Availability::Error, "error"),
        ] {
            let check = DailyFacts {
                availability,
                ..facts()
            };
            let day = run(&booked("2026-10-20 15:00"), &check, "2026-10-10 06:00");
            assert_eq!(day.outcome.commitment.deadline_at, at("2026-11-01 11:00"));
            assert_eq!(day.outcome.events[0].detail["reason"], reason);
        }
    }

    #[test]
    fn available_day_is_marked_checked_without_an_event() {
        let available = DailyFacts {
            availability: Availability::Available,
            ..facts()
        };
        let day = run(&base(), &available, "2026-10-10 06:00");
        let c = &day.outcome.commitment;
        assert_eq!(c.availability_checked_on, Some(date("2026-10-10")));
        assert_eq!(c.deadline_at, at("2026-10-31 10:00"));
        assert!(day.outcome.events.is_empty());
    }

    #[test]
    fn unchecked_day_neither_tolls_nor_marks() {
        let day = run(&base(), &facts(), "2026-10-10 06:00");
        assert_eq!(day.outcome, unchanged(&base()));
    }

    #[test]
    fn no_tolling_once_the_deadline_has_come() {
        let no_slots = DailyFacts {
            availability: Availability::NoSlots,
            ..facts()
        };
        let day = run(&base(), &no_slots, "2026-10-31 10:00");
        assert_eq!(day.outcome, cycle_started(&base(), "2026-10-31 10:00"));
        assert_eq!(day.due, [Due::Notice]);
    }

    #[test]
    fn deadline_makes_the_notice_due() {
        assert_eq!(
            due_at(&base(), "2026-10-31 09:59"),
            [Due::Reminder(ReminderStep::Day27)]
        );
        let day = run(&base(), &facts(), "2026-10-31 10:00");
        assert_eq!(day.outcome, cycle_started(&base(), "2026-10-31 10:00"));
        assert_eq!(day.due, [Due::Notice]);
    }

    #[test]
    fn a_deadline_moved_into_the_future_ends_the_notice_cycle() {
        // Due but not sent yet (no Manager reached), then a missed day tolls.
        let due = run(&base(), &facts(), "2026-10-31 11:00")
            .outcome
            .commitment;
        let tolled = DailyFacts {
            earlier_days: vec![(date("2026-10-31"), Availability::NoSlots)],
            ..facts()
        };
        let day = run(&due, &tolled, "2026-11-01 09:00");
        assert_eq!(day.outcome.commitment.deadline_at, at("2026-11-01 11:00"));
        assert_eq!(day.outcome.commitment.notice_cycle_at, None);
        assert_eq!(day.due, [Due::Reminder(ReminderStep::Day27)]);
        let next = run(&day.outcome.commitment, &facts(), "2026-11-01 11:30");
        assert_eq!(next.due, [Due::Notice]);
        assert_eq!(
            next.outcome.commitment.notice_cycle_at,
            Some(at("2026-11-01 11:30"))
        );
    }

    #[test]
    fn notice_waits_for_a_call_inside_its_no_show_window() {
        // Ends 09:00, before the 10:00 deadline.
        let c = booked("2026-10-31 08:00");
        for now in ["2026-10-31 10:00", "2026-11-02 08:59"] {
            let day = run(&c, &facts(), now);
            assert_eq!(day.outcome, unchanged(&c));
            assert!(day.due.is_empty());
        }
        let day = run(&c, &facts(), "2026-11-02 09:00");
        assert_eq!(day.outcome.commitment.status, Status::Completed);
    }

    #[test]
    fn no_show_inside_the_window_makes_the_notice_due_at_once() {
        let c = booked("2026-10-31 08:00");
        let mark = with(BookingCheck::Active {
            no_show_at: Some(at("2026-11-01 09:00")),
        });
        let day = run(&c, &mark, "2026-11-01 10:00");
        assert_eq!(kinds(&day.outcome), [EventKind::NoShow]);
        assert_eq!(day.due, [Due::Notice]);
    }

    #[test]
    fn no_consent_at_the_deadline_skips_the_advance() {
        let no_consent = DailyFacts {
            consent_on_file: false,
            ..facts()
        };
        let day = run(&base(), &no_consent, "2026-10-31 10:00");
        assert_eq!(day.outcome.commitment.status, Status::AdvanceSkipped);
        assert_eq!(day.outcome.events[0].detail["reason"], "no_consent");
        assert!(day.due.is_empty());
    }

    #[test]
    fn charging_off_at_the_deadline_skips_the_advance_for_good() {
        let off = DailyFacts {
            charging_enabled: false,
            ..facts()
        };
        let day = run(&base(), &off, "2026-10-31 10:00");
        let c = &day.outcome.commitment;
        assert_eq!(c.status, Status::AdvanceSkipped);
        assert_eq!(kinds(&day.outcome), [EventKind::AdvanceSkipped]);
        assert_eq!(day.outcome.events[0].detail["reason"], "charging_off");
        assert!(day.due.is_empty());

        let later = run(c, &facts(), "2026-11-10 10:00");
        assert_eq!(later.outcome, unchanged(c));
        assert!(later.due.is_empty());
    }

    #[test]
    fn reminders_fall_due_on_days_7_21_and_27() {
        let c = base();
        assert!(due_at(&c, "2026-10-08 09:59").is_empty());
        for (now, step) in [
            ("2026-10-08 10:00", ReminderStep::Day7),
            ("2026-10-22 09:59", ReminderStep::Day7),
            ("2026-10-22 10:00", ReminderStep::Day21),
            ("2026-10-28 09:59", ReminderStep::Day21),
            ("2026-10-28 10:00", ReminderStep::Day27),
            ("2026-10-31 09:59", ReminderStep::Day27),
        ] {
            assert_eq!(due_at(&c, now), [Due::Reminder(step)], "at {now}");
        }
        assert_eq!(due_at(&c, "2026-10-31 10:00"), [Due::Notice]);
    }

    #[test]
    fn missed_reminder_is_never_sent_once_the_next_is_due() {
        // The job was down from day 6 to day 22: only day 21 goes out.
        assert_eq!(
            due_at(&base(), "2026-10-23 10:00"),
            [Due::Reminder(ReminderStep::Day21)]
        );
    }

    #[test]
    fn booked_companies_get_the_last_reminder_until_a_moved_deadline() {
        let c = Commitment {
            deadline_at: at("2026-11-03 10:00"),
            ..booked("2026-11-02 15:00")
        };
        assert_eq!(
            due_at(&c, "2026-11-02 10:00"),
            [Due::Reminder(ReminderStep::Day27)]
        );
    }

    #[test]
    fn extension_adds_fourteen_days_once() {
        let out = superuser_action(&base(), Action::Extend, ACTOR, at("2026-10-10 10:00")).unwrap();
        let c = &out.commitment;
        assert_eq!(c.deadline_at, at("2026-11-14 11:00"));
        assert_eq!(c.original_deadline_at, at("2026-10-31 10:00"));
        assert!(c.extension_used);
        assert_eq!(kinds(&out), [EventKind::Extended]);
        assert_eq!(out.events[0].actor, Some(ACTOR));
        assert_eq!(out.events[0].detail["days_added"], 14);
        let again = superuser_action(c, Action::Extend, ACTOR, at("2026-10-11 10:00"));
        assert_eq!(again, Err(Refusal::AlreadyExtended));
    }

    #[test]
    fn waive_and_complete_record_the_superuser() {
        let c = booked("2026-10-20 15:00");
        let now = at("2026-10-10 10:00");
        let waived = superuser_action(&c, Action::Waive, ACTOR, now).unwrap();
        assert_eq!(waived.commitment.status, Status::Waived);
        assert_eq!(kinds(&waived), [EventKind::Waived]);
        assert_eq!(waived.events[0].actor, Some(ACTOR));
        assert_eq!(waived.events[0].detail["by"], "superuser");

        let done = superuser_action(&c, Action::Complete, ACTOR, now).unwrap();
        assert_eq!(done.commitment.status, Status::Completed);
        assert_eq!(done.commitment.completed_at, Some(now));
        assert_eq!(done.commitment.completed_by, Some(ACTOR));
        assert_eq!(done.events[0].actor, Some(ACTOR));
        assert_eq!(done.events[0].detail["basis"], "superuser");
    }

    #[test]
    fn superuser_no_show_unbooks_inside_the_window() {
        let c = booked("2026-10-20 15:00");
        let out = superuser_action(&c, Action::NoShow, ACTOR, at("2026-10-20 15:30")).unwrap();
        assert_eq!(out.commitment.status, Status::Pending);
        assert_eq!(out.commitment.booking, None);
        assert_eq!(out.commitment.no_show_at, Some(at("2026-10-20 15:30")));
        assert_eq!(
            out.commitment.no_show_call_start_at,
            Some(at("2026-10-20 15:00"))
        );
        assert_eq!(kinds(&out), [EventKind::NoShow]);
        assert_eq!(out.events[0].actor, Some(ACTOR));
        assert_eq!(out.events[0].detail["source"], "superuser");
        let last_moment = superuser_action(&c, Action::NoShow, ACTOR, at("2026-10-22 16:00"));
        assert!(last_moment.is_ok());
    }

    #[test]
    fn superuser_no_show_refused_without_a_call_before_it_starts_or_after_its_window() {
        let none = superuser_action(&base(), Action::NoShow, ACTOR, at("2026-10-20 16:00"));
        assert_eq!(none, Err(Refusal::NotBooked));
        let c = booked("2026-10-20 15:00");
        let early = superuser_action(&c, Action::NoShow, ACTOR, at("2026-10-20 14:59"));
        assert_eq!(early, Err(Refusal::CallNotStarted));
        let late = superuser_action(&c, Action::NoShow, ACTOR, at("2026-10-22 16:01"));
        assert_eq!(late, Err(Refusal::NoShowTooLate));
    }

    #[test]
    fn a_local_day_ends_at_23_59_59_local_on_either_side_of_a_dst_change() {
        for (day, end) in [
            ("2026-10-31", "2026-11-01 03:59:59"),
            ("2026-11-01", "2026-11-02 04:59:59"),
            ("2027-03-13", "2027-03-14 04:59:59"),
            ("2027-03-14", "2027-03-15 03:59:59"),
        ] {
            assert_eq!(end_of_local_day(date(day)), secs(end), "{day}");
        }
    }

    #[test]
    fn the_signup_deadline_is_the_end_of_the_day_the_consent_named() {
        // 15:00 EDT on 10-23: by November 22.
        assert_eq!(
            signup_deadline_at(secs("2026-10-23 19:00:00")),
            secs("2026-11-23 04:59:59")
        );
        // 22:30 EDT on 10-23 is 10-24 in UTC: still the local 10-23.
        assert_eq!(
            signup_deadline_at(secs("2026-10-24 02:30:00")),
            secs("2026-11-23 04:59:59")
        );
        // 23:30 EST on 02-12: by March 14, past the March 14 DST change
        // (signup + 30 x 24 hours would be 00:30 EDT on March 15).
        assert_eq!(
            signup_deadline_at(secs("2027-02-13 04:30:00")),
            secs("2027-03-15 03:59:59")
        );
    }

    #[test]
    fn local_days_keep_the_shown_date_and_wall_time_across_dst() {
        let oct31 = end_of_local_day(date("2026-10-31"));
        assert_eq!(
            add_local_days(oct31, 1),
            end_of_local_day(date("2026-11-01"))
        );
        assert_eq!(
            add_local_days(oct31, EXTENSION_DAYS),
            end_of_local_day(date("2026-11-14"))
        );
        let mar13 = end_of_local_day(date("2027-03-13"));
        assert_eq!(
            add_local_days(mar13, 1),
            end_of_local_day(date("2027-03-14"))
        );
        assert_eq!(add_local_days(mar13, 0), mar13);
        // 02:30 EST on 03-13 has no 03-14 twin: 03:30 EDT.
        assert_eq!(
            add_local_days(secs("2027-03-13 07:30:00"), 1),
            secs("2027-03-14 07:30:00")
        );
    }

    #[test]
    fn tolling_extension_and_host_days_move_the_local_day_end_across_dst() {
        let c = Commitment {
            deadline_at: end_of_local_day(date("2026-10-31")),
            ..booked("2026-10-20 15:00")
        };
        let no_slots = DailyFacts {
            availability: Availability::NoSlots,
            ..facts()
        };
        let tolled = run(&c, &no_slots, "2026-10-10 06:00").outcome.commitment;
        assert_eq!(tolled.deadline_at, end_of_local_day(date("2026-11-01")));
        let extended = superuser_action(&tolled, Action::Extend, ACTOR, at("2026-10-11 10:00"))
            .unwrap()
            .commitment;
        assert_eq!(extended.deadline_at, end_of_local_day(date("2026-11-15")));
        let moved = with(BookingCheck::MovedByHost(call("2026-10-22 16:00")));
        let host = run(&extended, &moved, "2026-10-12 10:00")
            .outcome
            .commitment;
        assert_eq!(host.deadline_at, end_of_local_day(date("2026-11-18")));

        let march = Commitment {
            deadline_at: end_of_local_day(date("2027-03-13")),
            ..base()
        };
        let tolled = run(&march, &no_slots, "2027-03-01 06:00")
            .outcome
            .commitment;
        assert_eq!(tolled.deadline_at, end_of_local_day(date("2027-03-14")));
    }
}
