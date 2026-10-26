//! The onboarding commitment's rules (Terms section 11), pure: every
//! transition takes the commitment, the facts and `now`, and returns the new
//! commitment plus the timeline events to store. No I/O, no clock.

use crate::billing::catalog::{SIGNUP_TZ, deadline_on, signup_day};
use chrono::{Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone};
use serde_json::{Value, json};

pub const DEADLINE_DAYS: i64 = 30;
pub const EXTENSION_DAYS: i64 = 14;
/// A no-show recorded later than this after the call's end does not count.
pub const NO_SHOW_WINDOW_HOURS: i64 = 48;
/// The fee is charged this long after its notice email.
pub const NOTICE_DAYS: i64 = 7;
/// Reminder emails, in days after signup.
pub const REMINDER_DAYS: [i64; 3] = [7, 21, 27];
/// Tolling: a day tolls when no slot is bookable within this many business
/// days (Mon to Fri, local to `SIGNUP_TZ`) after it.
pub const SLOT_WINDOW_BUSINESS_DAYS: u32 = 5;

const SECS_PER_DAY: i64 = 86_400;

/// `company_onboarding_commitments.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Status {
    Pending,
    Booked,
    Completed,
    Waived,
    Suspended,
    NoticeSent,
    FeeInvoiced,
    FeePaid,
    FeeFailed,
    FeeSkipped,
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
            Self::FeeInvoiced => "fee_invoiced",
            Self::FeePaid => "fee_paid",
            Self::FeeFailed => "fee_failed",
            Self::FeeSkipped => "fee_skipped",
            Self::Void => "void",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "booked" => Some(Self::Booked),
            "completed" => Some(Self::Completed),
            "waived" => Some(Self::Waived),
            "suspended" => Some(Self::Suspended),
            "notice_sent" => Some(Self::NoticeSent),
            "fee_invoiced" => Some(Self::FeeInvoiced),
            "fee_paid" => Some(Self::FeePaid),
            "fee_failed" => Some(Self::FeeFailed),
            "fee_skipped" => Some(Self::FeeSkipped),
            "void" => Some(Self::Void),
            _ => None,
        }
    }

    /// Nothing changes it any more: completed, waived, fee paid, fee skipped, void.
    pub const fn is_final(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Waived | Self::FeePaid | Self::FeeSkipped | Self::Void
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
    pub fee_cents: i64,
    pub booking: Option<Booking>,
    pub completed_at: Option<NaiveDateTime>,
    pub completed_by: Option<i32>,
    pub no_show_at: Option<NaiveDateTime>,
    /// Start of the call the latest no-show was for: that call and older
    /// ones never count again.
    pub no_show_call_start_at: Option<NaiveDateTime>,
    pub suspended_at: Option<NaiveDateTime>,
    /// Kept while suspended: a withdrawal before `charge_after` resumes the
    /// notice with its charge date.
    pub notice_sent_at: Option<NaiveDateTime>,
    /// When the current fee notice fell due (`daily` sets it): the notice
    /// emails are deduped per cycle, so a withdrawn and later resumed notice
    /// is sent again. Cleared by a suspension, its withdrawal or a moved
    /// deadline.
    pub notice_cycle_at: Option<NaiveDateTime>,
    pub charge_after: Option<NaiveDateTime>,
    pub stripe_invoice_id: Option<String>,
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
    Invoiced,
    FeePaid,
    FeeFailed,
    FeeSkipped,
    Voided,
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
            Self::Invoiced => "invoiced",
            Self::FeePaid => "fee_paid",
            Self::FeeFailed => "fee_failed",
            Self::FeeSkipped => "fee_skipped",
            Self::Voided => "voided",
        }
    }
}

/// A timeline entry: `detail` holds the facts behind it (reason, old and
/// new deadline, call times, invoice id).
#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub commitment: Commitment,
    pub events: Vec<Event>,
}

/// Why an action is refused (the routes answer 409 with `message()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Completed, waived, paid, skipped or void.
    Final,
    /// The fee invoice exists: void or refund it in Stripe instead.
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
}

impl Refusal {
    pub const fn message(self) -> &'static str {
        match self {
            Self::Final => "Onboarding is already settled for this company.",
            Self::AlreadyInvoiced => {
                "The onboarding fee is already invoiced. Void or refund it in Stripe."
            }
            Self::AlreadyExtended => "The 14-day extension was already used.",
            Self::ExtensionStillPast => "Even with 14 more days the deadline would be in the past.",
            Self::NotBooked => "No onboarding call is booked.",
            Self::CallNotStarted => "The booked call has not started yet.",
            Self::NoShowTooLate => "The call ended more than 48 hours ago, so it counts as held.",
            Self::MissedCall => "This call was marked as missed. Please book a new time.",
        }
    }
}

/// A superuser's action (`PUT /v1/superuser/companies/{id}/onboarding-commitment`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// The one 14-day extension the customer asked for.
    Extend,
    /// No fee: Granite's decision (a dispute, a goodwill gesture).
    Waive,
    /// Onboarding done (say, on a call outside Calendly).
    Complete,
    /// The customer missed the booked call.
    NoShow,
}

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
    /// The days after `availability_checked_on` (or signup) and before today,
    /// oldest first, with the check recorded for each (`NotChecked` when the
    /// job did not run that day). Any day but `Available` tolls: a day we
    /// cannot show a slot for counts against us.
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
    /// `ONBOARDING_FEE_CHARGING=1`.
    pub charging_enabled: bool,
    /// The company accepted the `onboarding_fee` consent.
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

/// Work the daily run must do after saving `outcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Due {
    /// Email this reminder (deduped per manager in `billing_reminder_emails`).
    Reminder(ReminderStep),
    /// Email the fee notice (deduped per `notice_cycle_at`, which `daily`
    /// has set); on a sent email call `notice_sent`.
    Notice,
    /// Create the fee invoice; then call `invoice_created`.
    Invoice,
}

#[derive(Debug, Clone, PartialEq)]
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

const fn is_invoiced(status: Status) -> bool {
    matches!(status, Status::FeeInvoiced | Status::FeeFailed)
}

/// Settled and invoiced commitments refuse every action.
const fn check_open(status: Status) -> Result<(), Refusal> {
    if status.is_final() {
        Err(Refusal::Final)
    } else if is_invoiced(status) {
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

fn no_show_window_end(call: &Booking) -> NaiveDateTime {
    call.end_at + Duration::hours(NO_SHOW_WINDOW_HOURS)
}

/// A call that counts (ends by `limit`) and is still inside its no-show
/// window: the fee waits for it.
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
/// is dropped with its charge date (a sent one goes back to booked or
/// pending; a new notice naming the new deadline, with a new charge date at
/// least 7 days on, falls due once it passes). Suspended stays suspended.
/// With the deadline in the future again the notice cycle ends too.
fn withdraw_stale_notice(commitment: &mut Commitment, now: NaiveDateTime) -> bool {
    if matches!(commitment.status, Status::Suspended | Status::NoticeSent) {
        let dropped = commitment.notice_sent_at.is_some();
        if commitment.status == Status::NoticeSent {
            commitment.status = resting_status(commitment);
        }
        commitment.notice_sent_at = None;
        commitment.charge_after = None;
        commitment.notice_cycle_at = None;
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
    fee_cents: i64,
) -> Outcome {
    let commitment = Commitment {
        company_id,
        status: Status::Pending,
        created_at: signup_at,
        deadline_at,
        original_deadline_at: deadline_at,
        tolled_days: 0,
        extension_used: false,
        fee_cents,
        booking: None,
        completed_at: None,
        completed_by: None,
        no_show_at: None,
        no_show_call_start_at: None,
        suspended_at: None,
        notice_sent_at: None,
        notice_cycle_at: None,
        charge_after: None,
        stripe_invoice_id: None,
        availability_checked_on: None,
    };
    let created = Event {
        kind: EventKind::Created,
        from: None,
        to: Status::Pending,
        deadline_at,
        actor: None,
        detail: json!({ "fee_cents": fee_cents, "deadline_at": ts(deadline_at) }),
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
/// invitee. Replaces a stored booking (`Rescheduled`). A call starting at or
/// before the missed call's start is refused.
pub fn record_booking(
    commitment: &Commitment,
    booking: Booking,
    _now: NaiveDateTime,
) -> Result<Outcome, Refusal> {
    check_open(commitment.status)?;
    if is_missed(commitment, &booking) {
        return Err(Refusal::MissedCall);
    }
    let mut out = unchanged(commitment);
    store_booking(&mut out, booking, "widget");
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

/// Step 3: the stored call ended in time and its no-show window is over.
fn auto_complete(out: &mut Outcome, now: NaiveDateTime) -> bool {
    let c = &out.commitment;
    let Some(call) = c.booking.as_ref() else {
        return false;
    };
    let counts = match c.status {
        Status::Pending | Status::Booked => call.end_at <= c.deadline_at,
        Status::NoticeSent => call.end_at <= c.charge_after.unwrap_or(c.deadline_at),
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
/// deadline), and one before the charge date while a notice is out (the
/// notice tells the customer that cancelling before then avoids the fee).
fn request_suspends(c: &Commitment, requested: NaiveDateTime) -> bool {
    match c.status {
        Status::Pending | Status::Booked => true,
        Status::NoticeSent => c
            .charge_after
            .is_none_or(|charge_after| requested < charge_after),
        _ => false,
    }
}

/// A suspension withdrawn at `withdrawn_at` resumes: before the deadline
/// the countdown (a notice is dropped); after it a notice sent after the
/// deadline, when withdrawn before its charge date (the fee is due on it,
/// at once when that has passed since); otherwise a new notice falls due,
/// at least 7 days before any charge (Terms 11.6).
fn resume(c: &mut Commitment, withdrawn_at: NaiveDateTime, now: NaiveDateTime) -> &'static str {
    c.suspended_at = None;
    c.notice_cycle_at = None;
    // Only a notice sent after the current deadline passed is about it.
    let notice_holds = now >= c.deadline_at
        && c.charge_after
            .is_some_and(|charge_after| withdrawn_at < charge_after)
        && c.notice_sent_at.is_some_and(|sent| sent >= c.deadline_at);
    if notice_holds {
        c.status = Status::NoticeSent;
        return "notice_sent";
    }
    c.notice_sent_at = None;
    c.charge_after = None;
    c.status = resting_status(c);
    if now < c.deadline_at {
        "countdown"
    } else {
        "notice"
    }
}

/// Suspends for a request made at `requested`; the notice and its charge
/// date are kept for [`resume`], its email cycle ends.
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

/// A request made during the notice period and withdrawn on or after the
/// charge date with no run in between (never suspended): the notice is
/// dropped as [`resume`] drops it, and a new one falls due.
fn withdrawn_late(out: &mut Outcome, facts: &DailyFacts) {
    let c = &out.commitment;
    let (Status::NoticeSent, Some(sent), Some(charge_after)) =
        (c.status, c.notice_sent_at, c.charge_after)
    else {
        return;
    };
    let Some(&(requested, withdrawn)) =
        facts
            .withdrawn_cancellations
            .iter()
            .find(|(requested, withdrawn)| {
                *requested < charge_after && *withdrawn >= charge_after && *withdrawn > sent
            })
    else {
        return;
    };
    let c = &mut out.commitment;
    c.notice_sent_at = None;
    c.charge_after = None;
    c.notice_cycle_at = None;
    c.status = resting_status(c);
    let detail = json!({
        "resumes_at": "notice",
        "charge_after": null,
        "requested_at": ts(requested),
        "withdrawn_at": ts(withdrawn),
    });
    push(out, EventKind::Resumed, Status::NoticeSent, None, detail);
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
            let resumes_at = resume(&mut out.commitment, withdrawn_at, now);
            let c = &out.commitment;
            let detail = json!({
                "resumes_at": resumes_at,
                "charge_after": c.charge_after.map(ts),
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

/// Step 5: each day with no bookable slot moves the deadline by one day,
/// once: the earlier days the job missed, then today.
fn toll(out: &mut Outcome, facts: &DailyFacts, now: NaiveDateTime) {
    let today = now.date();
    for (day, availability) in &facts.earlier_days {
        let c = &out.commitment;
        if !matches!(c.status, Status::Pending | Status::Booked)
            || *day >= today
            || day.and_time(NaiveTime::MIN) >= c.deadline_at
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

/// Steps 6 and 7: past the deadline the notice falls due, past the charge
/// date the invoice; either is skipped for good when charging is off or no
/// consent is on file, and waits for a call inside its no-show window.
fn fee_due(out: &mut Outcome, facts: &DailyFacts, now: NaiveDateTime) -> Option<Due> {
    let c = &out.commitment;
    let (limit, due) = match c.status {
        Status::Pending | Status::Booked => (c.deadline_at, Due::Notice),
        Status::NoticeSent => (c.charge_after?, Due::Invoice),
        _ => return None,
    };
    if now < limit || call_awaiting(c.booking.as_ref(), limit, now) {
        return None;
    }
    let reason = match (facts.charging_enabled, facts.consent_on_file) {
        (true, true) => return Some(due),
        (false, _) => "charging_off",
        (true, false) => "no_consent",
    };
    let from = c.status;
    let detail = json!({ "reason": reason, "fee_cents": c.fee_cents });
    out.commitment.status = Status::FeeSkipped;
    push(out, EventKind::FeeSkipped, from, None, detail);
    None
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
    if from.is_final() || is_invoiced(from) {
        return Vec::new();
    }
    if facts.subscription_ended {
        out.commitment.status = Status::Void;
        let detail = json!({ "reason": "subscription_ended" });
        push(out, EventKind::Voided, from, None, detail);
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
    if let Some(due) = fee_due(out, facts, now) {
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

/// The daily run for one company: booking refresh, void, auto-complete,
/// suspension and its withdrawal, tolling, the deadline (notice or skip),
/// the charge date, reminders. A notice falling due starts its cycle
/// (`notice_cycle_at`); save the outcome before sending it.
pub fn daily(commitment: &Commitment, facts: &DailyFacts, now: NaiveDateTime) -> Daily {
    let mut outcome = unchanged(commitment);
    let due = run_daily(&mut outcome, facts, now);
    Daily { outcome, due }
}

/// The fee notice went out to at least one manager.
pub fn notice_sent(commitment: &Commitment, now: NaiveDateTime) -> Outcome {
    let mut out = unchanged(commitment);
    let from = commitment.status;
    if !matches!(from, Status::Pending | Status::Booked) {
        return out;
    }
    let charge_after = now + Duration::days(NOTICE_DAYS);
    out.commitment.status = Status::NoticeSent;
    out.commitment.notice_sent_at = Some(now);
    out.commitment.charge_after = Some(charge_after);
    let detail = json!({
        "notice_sent_at": ts(now),
        "charge_after": ts(charge_after),
        "fee_cents": commitment.fee_cents,
    });
    push(&mut out, EventKind::NoticeSent, from, None, detail);
    out
}

/// The fee invoice exists in Stripe (finalized or not).
pub fn invoice_created(commitment: &Commitment, invoice_id: &str, _now: NaiveDateTime) -> Outcome {
    let mut out = unchanged(commitment);
    let from = commitment.status;
    if from != Status::NoticeSent {
        return out;
    }
    out.commitment.status = Status::FeeInvoiced;
    out.commitment.stripe_invoice_id = Some(invoice_id.to_string());
    let detail = json!({ "invoice_id": invoice_id, "fee_cents": commitment.fee_cents });
    push(&mut out, EventKind::Invoiced, from, None, detail);
    out
}

/// What is checked right before the fee invoice reaches Stripe (created,
/// finalized, paid).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChargeFacts {
    /// `ONBOARDING_FEE_CHARGING=1`.
    pub charging_enabled: bool,
    /// The company accepted the `onboarding_fee` consent.
    pub consent_on_file: bool,
    /// The subscription is cancelled for real, or the company is deleted.
    pub subscription_ended: bool,
    /// As in [`DailyFacts::cancellation_requested_at`].
    pub cancellation_requested_at: Option<NaiveDateTime>,
}

/// Why the fee invoice may not be charged: it is deleted (a draft) or voided
/// in Stripe, then [`invoice_withdrawn`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Withdrawal {
    /// The subscription ended: void.
    SubscriptionEnded,
    /// A cancellation request made before the charge date: suspended.
    CancellationRequested(NaiveDateTime),
    /// `ONBOARDING_FEE_CHARGING` is off: skipped for good.
    ChargingOff,
    /// No `onboarding_fee` consent on file: skipped for good.
    NoConsent,
}

impl Withdrawal {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SubscriptionEnded => "subscription_ended",
            Self::CancellationRequested(_) => "cancellation_requested",
            Self::ChargingOff => "charging_off",
            Self::NoConsent => "no_consent",
        }
    }
}

/// Why the fee may not reach Stripe now, in the daily run's order (void,
/// suspension, skip); `None` when it may.
pub fn withdrawal(commitment: &Commitment, facts: &ChargeFacts) -> Option<Withdrawal> {
    let charge_at = commitment.charge_after.unwrap_or(commitment.deadline_at);
    if facts.subscription_ended {
        Some(Withdrawal::SubscriptionEnded)
    } else if let Some(requested) = facts.cancellation_requested_at
        && requested < charge_at
    {
        Some(Withdrawal::CancellationRequested(requested))
    } else if !facts.charging_enabled {
        Some(Withdrawal::ChargingOff)
    } else if !facts.consent_on_file {
        Some(Withdrawal::NoConsent)
    } else {
        None
    }
}

/// The fee invoice was deleted or voided in Stripe before any charge:
/// skipped for good (charging off, no consent), void (the subscription
/// ended) or suspended (the notice and its charge date kept for a
/// withdrawal of the request). The invoice id is cleared, so its Stripe
/// events change nothing. Only from `fee_invoiced`.
pub fn invoice_withdrawn(
    commitment: &Commitment,
    reason: Withdrawal,
    now: NaiveDateTime,
) -> Outcome {
    let mut out = unchanged(commitment);
    let from = commitment.status;
    if from != Status::FeeInvoiced {
        return out;
    }
    let invoice_id = out.commitment.stripe_invoice_id.take();
    let detail = json!({
        "reason": reason.as_str(),
        "invoice_withdrawn": invoice_id,
        "fee_cents": commitment.fee_cents,
    });
    let kind = match reason {
        Withdrawal::CancellationRequested(requested) => {
            suspend(&mut out, requested, now, detail);
            return out;
        }
        Withdrawal::SubscriptionEnded => {
            out.commitment.status = Status::Void;
            EventKind::Voided
        }
        Withdrawal::ChargingOff | Withdrawal::NoConsent => {
            out.commitment.status = Status::FeeSkipped;
            EventKind::FeeSkipped
        }
    };
    push(&mut out, kind, from, None, detail);
    out
}

/// A Stripe event about the fee invoice: `invoice.paid`, `invoice.payment_failed`
/// or `invoice.voided`. Unchanged when `invoice_id` is not the stored one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvoiceEvent {
    Paid,
    Failed,
    Voided,
}

pub fn invoice_event(
    commitment: &Commitment,
    invoice_id: &str,
    event: InvoiceEvent,
    _now: NaiveDateTime,
) -> Outcome {
    let mut out = unchanged(commitment);
    if commitment.stripe_invoice_id.as_deref() != Some(invoice_id) {
        return out;
    }
    let from = commitment.status;
    let (to, kind, detail) = match (from, event) {
        (Status::FeeInvoiced | Status::FeeFailed, InvoiceEvent::Paid) => (
            Status::FeePaid,
            EventKind::FeePaid,
            json!({ "invoice_id": invoice_id, "fee_cents": commitment.fee_cents }),
        ),
        (Status::FeeInvoiced, InvoiceEvent::Failed) => (
            Status::FeeFailed,
            EventKind::FeeFailed,
            json!({ "invoice_id": invoice_id }),
        ),
        (Status::FeeInvoiced | Status::FeeFailed, InvoiceEvent::Voided) => (
            Status::Waived,
            EventKind::Waived,
            json!({ "by": "stripe_void", "invoice_id": invoice_id }),
        ),
        _ => return out,
    };
    out.commitment.status = to;
    push(&mut out, kind, from, None, detail);
    out
}

/// A superuser's action, stamped with `actor`.
pub fn superuser_action(
    commitment: &Commitment,
    action: Action,
    actor: i32,
    now: NaiveDateTime,
) -> Result<Outcome, Refusal> {
    check_open(commitment.status)?;
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
            let detail = json!({ "by": "superuser", "fee_cents": commitment.fee_cents });
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
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACTOR: i32 = 9;
    const ALL: [Status; 11] = [
        Status::Pending,
        Status::Booked,
        Status::Completed,
        Status::Waived,
        Status::Suspended,
        Status::NoticeSent,
        Status::FeeInvoiced,
        Status::FeePaid,
        Status::FeeFailed,
        Status::FeeSkipped,
        Status::Void,
    ];
    const SETTLED: [Status; 5] = [
        Status::Completed,
        Status::Waived,
        Status::FeePaid,
        Status::FeeSkipped,
        Status::Void,
    ];
    const INVOICED: [Status; 2] = [Status::FeeInvoiced, Status::FeeFailed];

    fn at(text: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").unwrap()
    }

    fn date(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
    }

    /// Signed up 2026-10-01 10:00, deadline 2026-10-31 10:00 (06:00 EDT).
    /// Moved past Nov 1, the same local time is 11:00 UTC (EST).
    fn base() -> Commitment {
        new_commitment(7, at("2026-10-01 10:00"), at("2026-10-31 10:00"), 60_000).commitment
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

    /// Notice sent 2026-10-31 12:00: charge after 2026-11-07 12:00.
    fn noticed(commitment: &Commitment) -> Commitment {
        notice_sent(commitment, at("2026-10-31 12:00")).commitment
    }

    fn invoiced() -> Commitment {
        invoice_created(&noticed(&base()), "in_1", at("2026-11-07 12:00")).commitment
    }

    const fn facts() -> DailyFacts {
        DailyFacts {
            booking: BookingCheck::None,
            availability: Availability::NotChecked,
            earlier_days: Vec::new(),
            cancellation_requested_at: None,
            withdrawn_cancellations: Vec::new(),
            subscription_ended: false,
            charging_enabled: true,
            consent_on_file: true,
        }
    }

    fn with(booking: BookingCheck) -> DailyFacts {
        let mut facts = facts();
        facts.booking = booking;
        facts
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

    // Strings

    #[test]
    fn statuses_round_trip_and_five_are_final() {
        for status in ALL {
            assert_eq!(Status::parse(status.as_str()), Some(status));
        }
        let finals: Vec<Status> = ALL.into_iter().filter(|s| s.is_final()).collect();
        assert_eq!(finals, SETTLED);
        assert_eq!(Status::parse("Pending"), None);
    }

    #[test]
    fn event_kinds_reminder_steps_and_refusals_have_their_strings() {
        assert_eq!(EventKind::BookingCancelled.as_str(), "booking_cancelled");
        assert_eq!(EventKind::NoShow.as_str(), "no_show");
        assert_eq!(EventKind::NoticeSent.as_str(), "notice_sent");
        assert_eq!(EventKind::Voided.as_str(), "voided");
        assert_eq!(
            REMINDER_STEPS.map(ReminderStep::as_str),
            ["day_7", "day_21", "day_27"]
        );
        assert_eq!(REMINDER_STEPS.map(ReminderStep::days), [7, 21, 27]);
        for refusal in [
            Refusal::Final,
            Refusal::AlreadyInvoiced,
            Refusal::AlreadyExtended,
            Refusal::ExtensionStillPast,
            Refusal::NotBooked,
            Refusal::CallNotStarted,
            Refusal::NoShowTooLate,
            Refusal::MissedCall,
        ] {
            assert!(!refusal.message().is_empty());
            assert!(!refusal.message().contains('\u{2014}'));
        }
    }

    // new_commitment

    #[test]
    fn new_commitment_is_pending_with_the_signup_deadline_frozen() {
        let out = new_commitment(7, at("2026-10-01 10:00"), at("2026-10-31 10:00"), 60_000);
        let c = &out.commitment;
        assert_eq!(c.status, Status::Pending);
        assert_eq!(c.created_at, at("2026-10-01 10:00"));
        assert_eq!(c.deadline_at, at("2026-10-31 10:00"));
        assert_eq!(c.original_deadline_at, at("2026-10-31 10:00"));
        assert_eq!(out.events.len(), 1);
        let created = &out.events[0];
        assert_eq!(created.kind, EventKind::Created);
        assert_eq!((created.from, created.to), (None, Status::Pending));
        assert_eq!(created.actor, None);
        assert_eq!(
            created.detail,
            json!({ "fee_cents": 60_000, "deadline_at": "2026-10-31T10:00:00Z" })
        );
    }

    // record_booking

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
    fn booking_keeps_notice_sent_and_suspended_and_the_no_show_history() {
        for status in [Status::NoticeSent, Status::Suspended] {
            let c = Commitment {
                no_show_at: Some(at("2026-10-15 12:00")),
                ..with_status(status)
            };
            let out = record_booking(&c, call("2026-11-03 15:00"), at("2026-11-01 09:00")).unwrap();
            assert_eq!(out.commitment.status, status);
            assert_eq!(out.commitment.booking, Some(call("2026-11-03 15:00")));
            assert_eq!(out.commitment.no_show_at, Some(at("2026-10-15 12:00")));
            assert_eq!(kinds(&out), [EventKind::Booked]);
        }
    }

    #[test]
    fn booking_refused_on_settled_and_invoiced_commitments() {
        let now = at("2026-11-10 10:00");
        for status in SETTLED {
            let refused = record_booking(&with_status(status), call("2026-11-20 15:00"), now);
            assert_eq!(refused, Err(Refusal::Final));
        }
        for status in INVOICED {
            let refused = record_booking(&with_status(status), call("2026-11-20 15:00"), now);
            assert_eq!(refused, Err(Refusal::AlreadyInvoiced));
        }
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

    // daily: settled, invoiced, void

    #[test]
    fn daily_leaves_settled_and_invoiced_commitments_alone() {
        let busy = DailyFacts {
            booking: BookingCheck::Found(call("2026-10-20 15:00")),
            availability: Availability::NoSlots,
            subscription_ended: true,
            ..facts()
        };
        for status in SETTLED.into_iter().chain(INVOICED) {
            let c = with_status(status);
            let day = run(&c, &busy, "2026-11-20 10:00");
            assert_eq!(day.outcome, unchanged(&c));
            assert!(day.due.is_empty());
        }
    }

    #[test]
    fn subscription_end_voids_every_live_state() {
        let ended = DailyFacts {
            subscription_ended: true,
            ..facts()
        };
        for status in [
            Status::Pending,
            Status::Booked,
            Status::Suspended,
            Status::NoticeSent,
        ] {
            let day = run(&with_status(status), &ended, "2026-11-02 10:00");
            assert_eq!(day.outcome.commitment.status, Status::Void);
            assert_eq!(kinds(&day.outcome), [EventKind::Voided]);
            assert_eq!(day.outcome.events[0].from, Some(status));
            assert!(day.due.is_empty());
        }
    }

    // daily: booking refresh

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
    fn host_cancellation_after_the_notice_withdraws_it_when_the_deadline_is_ahead_again() {
        let recorded = Booking {
            booked_at: at("2026-10-25 10:00"),
            ..call("2026-11-03 15:00")
        };
        let c = noticed(&book(&base(), recorded));
        assert_eq!(c.status, Status::NoticeSent);
        // Waited 7 days 2 hours: 8 days back, deadline 11-08 06:00 EST.
        let cancelled = with(BookingCheck::Cancelled {
            by_host: true,
            at: at("2026-11-01 12:00"),
        });
        let day = run(&c, &cancelled, "2026-11-01 13:00");
        let out = &day.outcome;
        assert_eq!(out.commitment.status, Status::Pending);
        assert_eq!(out.commitment.deadline_at, at("2026-11-08 11:00"));
        assert_eq!(out.commitment.notice_sent_at, None);
        assert_eq!(out.commitment.charge_after, None);
        assert_eq!(kinds(out), [EventKind::BookingCancelled]);
        assert_eq!(out.events[0].to, Status::Pending);
        assert_eq!(out.events[0].detail["notice_withdrawn"], true);
        assert_eq!(day.due, [Due::Reminder(ReminderStep::Day27)]);
    }

    #[test]
    fn host_cancellation_that_leaves_the_deadline_past_drops_the_notice_for_a_new_one() {
        let recorded = Booking {
            booked_at: at("2026-10-31 13:00"),
            ..call("2026-11-03 15:00")
        };
        let c = book(&noticed(&base()), recorded);
        let cancelled = with(BookingCheck::Cancelled {
            by_host: true,
            at: at("2026-11-01 12:00"),
        });
        let day = run(&c, &cancelled, "2026-11-01 13:00");
        let out = &day.outcome;
        assert_eq!(out.commitment.status, Status::Pending);
        assert_eq!(out.commitment.deadline_at, at("2026-11-01 11:00"));
        assert_eq!(out.commitment.charge_after, None);
        assert_eq!(out.events[0].detail["notice_withdrawn"], true);
        assert_eq!(day.due, [Due::Notice]);
    }

    #[test]
    fn a_host_move_during_the_notice_sends_a_new_notice_and_no_charge_on_the_old_date() {
        // Notice Nov 1 (charge Nov 8), call Nov 6; Granite moves it to Nov 9 on Nov 4.
        let notice = notice_sent(&base(), at("2026-11-01 12:00")).commitment;
        assert_eq!(notice.charge_after, Some(at("2026-11-08 12:00")));
        let c = book(
            &notice,
            Booking {
                booked_at: at("2026-11-01 13:00"),
                ..call("2026-11-06 15:00")
            },
        );
        let moved = with(BookingCheck::MovedByHost(Booking {
            booked_at: at("2026-11-04 09:00"),
            ..call("2026-11-09 15:00")
        }));
        let day = run(&c, &moved, "2026-11-04 10:00");
        let after = &day.outcome.commitment;
        assert_eq!(after.status, Status::Booked);
        assert_eq!((after.notice_sent_at, after.charge_after), (None, None));
        assert!(after.deadline_at < at("2026-11-04 10:00"));
        assert_eq!(day.due, [Due::Notice]);
        // The new notice dates the charge 7 days on; nothing is due on Nov 8.
        let renoticed = notice_sent(after, at("2026-11-04 10:00")).commitment;
        assert_eq!(renoticed.charge_after, Some(at("2026-11-11 10:00")));
        assert!(due_at(&renoticed, "2026-11-08 12:00").is_empty());
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

    // daily: auto-complete

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
    fn call_in_the_notice_period_completes_when_it_ends_by_the_charge_date() {
        let c = book(&noticed(&base()), call("2026-11-05 15:00"));
        let day = run(&c, &facts(), "2026-11-07 16:00");
        assert_eq!(day.outcome.commitment.status, Status::Completed);
        assert_eq!(day.outcome.events[0].from, Some(Status::NoticeSent));
        assert!(day.due.is_empty());
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

    // daily: suspension

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
    fn before_the_notice_a_request_after_the_deadline_suspends_and_no_notice_goes_out() {
        // Deadline 10-31 10:00; the notice was due (or not yet) but not sent.
        let due = cycle_started(&base(), "2026-10-31 11:00").commitment;
        for c in [base(), booked("2026-11-03 15:00"), due] {
            for requested_at in ["2026-10-31 10:00", "2026-11-01 09:00"] {
                let request = DailyFacts {
                    cancellation_requested_at: Some(at(requested_at)),
                    ..facts()
                };
                let day = run(&c, &request, "2026-11-01 10:00");
                let out = &day.outcome;
                assert_eq!(out.commitment.status, Status::Suspended);
                assert_eq!(out.commitment.notice_cycle_at, None);
                assert_eq!(
                    (out.commitment.notice_sent_at, out.commitment.charge_after),
                    (None, None)
                );
                assert_eq!(kinds(out), [EventKind::Suspended]);
                assert_eq!(out.events[0].detail["notice_held"], false);
                assert!(day.due.is_empty());
            }
        }
    }

    #[test]
    fn a_request_after_the_deadline_withdrawn_before_the_notice_gets_the_notice_then() {
        let suspended = run(&base(), &requested("2026-11-01 09:00"), "2026-11-01 10:00")
            .outcome
            .commitment;
        let day = run(&suspended, &facts(), "2026-11-03 10:00");
        let c = &day.outcome.commitment;
        assert_eq!(c.status, Status::Pending);
        assert_eq!(day.outcome.events[0].detail["resumes_at"], "notice");
        assert_eq!(c.notice_cycle_at, Some(at("2026-11-03 10:00")));
        assert_eq!(day.due, [Due::Notice]);
        let sent = notice_sent(c, at("2026-11-03 10:05")).commitment;
        assert_eq!(sent.charge_after, Some(at("2026-11-10 10:05")));
    }

    #[test]
    fn a_request_after_the_deadline_but_before_the_charge_date_suspends_and_holds_the_notice() {
        // Deadline 10-31 10:00, charge after 11-07 12:00.
        let c = Commitment {
            notice_cycle_at: Some(at("2026-10-31 11:00")),
            ..noticed(&base())
        };
        for (requested_at, now) in [
            ("2026-11-03 09:00", "2026-11-03 10:00"),
            ("2026-11-07 11:59", "2026-11-07 12:00"),
        ] {
            let request = DailyFacts {
                cancellation_requested_at: Some(at(requested_at)),
                ..facts()
            };
            let day = run(&c, &request, now);
            let out = &day.outcome;
            assert_eq!(out.commitment.status, Status::Suspended);
            assert_eq!(out.commitment.notice_sent_at, Some(at("2026-10-31 12:00")));
            assert_eq!(out.commitment.charge_after, Some(at("2026-11-07 12:00")));
            assert_eq!(out.commitment.notice_cycle_at, None);
            assert_eq!(kinds(out), [EventKind::Suspended]);
            assert_eq!(out.events[0].detail["notice_held"], true);
            assert!(day.due.is_empty());
        }
    }

    #[test]
    fn a_request_at_or_after_the_charge_date_does_not_suspend_the_notice() {
        let c = noticed(&base());
        for requested_at in ["2026-11-07 12:00", "2026-11-07 13:00"] {
            let request = DailyFacts {
                cancellation_requested_at: Some(at(requested_at)),
                ..facts()
            };
            let day = run(&c, &request, "2026-11-07 14:00");
            assert_eq!(day.outcome, unchanged(&c));
            assert_eq!(day.due, [Due::Invoice]);
        }
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
    fn withdrawal_after_the_deadline_resumes_at_the_notice_stage() {
        let c = Commitment {
            status: Status::Suspended,
            suspended_at: Some(at("2026-10-15 10:00")),
            ..base()
        };
        let day = run(&c, &facts(), "2026-11-02 10:00");
        assert_eq!(day.outcome.commitment.status, Status::Pending);
        assert_eq!(kinds(&day.outcome), [EventKind::Resumed]);
        assert_eq!(day.outcome.events[0].detail["resumes_at"], "notice");
        assert_eq!(day.due, [Due::Notice]);

        let off = DailyFacts {
            charging_enabled: false,
            ..facts()
        };
        let skipped = run(&c, &off, "2026-11-02 10:00");
        assert_eq!(skipped.outcome.commitment.status, Status::FeeSkipped);
        assert_eq!(
            kinds(&skipped.outcome),
            [EventKind::Resumed, EventKind::FeeSkipped]
        );
    }

    #[test]
    fn suspension_during_the_notice_period_keeps_its_charge_date() {
        let request = DailyFacts {
            cancellation_requested_at: Some(at("2026-10-31 09:00")),
            ..facts()
        };
        let day = run(&noticed(&base()), &request, "2026-11-01 10:00");
        let out = &day.outcome;
        assert_eq!(out.commitment.status, Status::Suspended);
        assert_eq!(out.commitment.notice_sent_at, Some(at("2026-10-31 12:00")));
        assert_eq!(out.commitment.charge_after, Some(at("2026-11-07 12:00")));
        assert_eq!(out.events[0].detail["notice_held"], true);
        assert!(day.due.is_empty());
    }

    fn requested(at_text: &str) -> DailyFacts {
        DailyFacts {
            cancellation_requested_at: Some(at(at_text)),
            ..facts()
        }
    }

    #[test]
    fn request_and_withdrawal_twice_during_a_notice_never_move_the_charge_date() {
        // Notice 10-31 12:00, charge after 11-07 12:00.
        let mut c = noticed(&base());
        for (request, suspended_on, resumed_on) in [
            ("2026-11-01 09:00", "2026-11-01 10:00", "2026-11-02 10:00"),
            ("2026-11-03 09:00", "2026-11-03 10:00", "2026-11-04 10:00"),
        ] {
            c = run(&c, &requested(request), suspended_on)
                .outcome
                .commitment;
            assert_eq!(c.status, Status::Suspended);
            let resumed = run(&c, &facts(), resumed_on);
            assert_eq!(kinds(&resumed.outcome), [EventKind::Resumed]);
            assert_eq!(
                resumed.outcome.events[0].detail["resumes_at"],
                "notice_sent"
            );
            assert!(resumed.due.is_empty());
            c = resumed.outcome.commitment;
            assert_eq!(c.status, Status::NoticeSent);
            assert_eq!(c.notice_sent_at, Some(at("2026-10-31 12:00")));
            assert_eq!(c.charge_after, Some(at("2026-11-07 12:00")));
        }
        assert_eq!(due_at(&c, "2026-11-07 12:00"), [Due::Invoice]);
    }

    #[test]
    fn a_deadline_moved_while_suspended_needs_a_new_notice_before_any_charge() {
        // Notice 10-31 12:00 (charge 11-07 12:00), request 11-02, extended on 11-03.
        let suspended = run(
            &noticed(&base()),
            &requested("2026-11-02 09:00"),
            "2026-11-02 10:00",
        )
        .outcome
        .commitment;
        let extended =
            superuser_action(&suspended, Action::Extend, ACTOR, at("2026-11-03 10:00")).unwrap();
        let c = &extended.commitment;
        assert_eq!(c.status, Status::Suspended);
        assert_eq!(c.deadline_at, at("2026-11-14 11:00"));
        assert_eq!((c.notice_sent_at, c.charge_after), (None, None));
        assert_eq!(extended.events[0].detail["notice_withdrawn"], true);

        let day = run(c, &facts(), "2026-11-20 10:00");
        let resumed = &day.outcome.commitment;
        assert_eq!(resumed.status, Status::Pending);
        assert_eq!(resumed.charge_after, None);
        assert_eq!(resumed.notice_cycle_at, Some(at("2026-11-20 10:00")));
        assert_eq!(day.due, [Due::Notice]);
    }

    #[test]
    fn a_notice_sent_before_the_current_deadline_is_never_resumed() {
        let mut held = run(
            &noticed(&base()),
            &requested("2026-11-02 09:00"),
            "2026-11-02 10:00",
        )
        .outcome
        .commitment;
        // As if the deadline had moved past the notice without clearing it.
        held.deadline_at = at("2026-11-05 10:00");
        let day = run(&held, &facts(), "2026-11-06 10:00");
        assert_eq!(day.outcome.commitment.status, Status::Pending);
        assert_eq!(day.outcome.commitment.charge_after, None);
        assert_eq!(day.due, [Due::Notice]);
    }

    /// Notice 10-31 12:00 (charge after 11-07 12:00), suspended by a
    /// request on 11-02.
    fn suspended_in_the_notice_week() -> Commitment {
        run(
            &noticed(&base()),
            &requested("2026-11-02 09:00"),
            "2026-11-02 10:00",
        )
        .outcome
        .commitment
    }

    /// The suspension withdrawn at `withdrawn` (requested 11-02 09:00).
    fn withdrawn_at(withdrawn: &str) -> DailyFacts {
        DailyFacts {
            withdrawn_cancellations: vec![(at("2026-11-02 09:00"), at(withdrawn))],
            ..facts()
        }
    }

    #[test]
    fn a_withdrawal_in_the_notice_week_keeps_the_charge_date_and_one_after_it_needs_a_new_notice() {
        let suspended = suspended_in_the_notice_week();
        // Withdrawn before the charge date: the notice and its date stand.
        let day = run(&suspended, &facts(), "2026-11-07 11:59");
        let c = &day.outcome.commitment;
        assert_eq!(c.status, Status::NoticeSent);
        assert_eq!(c.charge_after, Some(at("2026-11-07 12:00")));
        assert_eq!(day.outcome.events[0].detail["resumes_at"], "notice_sent");
        assert!(day.due.is_empty());
        assert_eq!(due_at(c, "2026-11-07 12:00"), [Due::Invoice]);

        // Withdrawn on or after it: no invoice, a new notice first.
        for now in ["2026-11-07 12:00", "2026-11-20 10:00", "2026-12-20 10:00"] {
            let day = run(&suspended, &facts(), now);
            let c = &day.outcome.commitment;
            assert_eq!(c.status, Status::Pending);
            assert_eq!((c.notice_sent_at, c.charge_after), (None, None));
            assert_eq!(c.notice_cycle_at, Some(at(now)));
            assert_eq!(kinds(&day.outcome), [EventKind::Resumed]);
            assert_eq!(day.outcome.events[0].detail["resumes_at"], "notice");
            assert_eq!(day.due, [Due::Notice]);
            let resent = notice_sent(c, at(now) + Duration::minutes(5)).commitment;
            assert_eq!(
                resent.charge_after,
                Some(at(now) + Duration::minutes(5) + Duration::days(7))
            );
        }
    }

    #[test]
    fn the_withdrawal_time_not_the_run_decides_whether_the_notice_holds() {
        let suspended = suspended_in_the_notice_week();
        // Withdrawn just before the charge date, seen by a run after it.
        let before = run(
            &suspended,
            &withdrawn_at("2026-11-07 11:00"),
            "2026-11-08 10:00",
        );
        assert_eq!(before.outcome.commitment.status, Status::NoticeSent);
        assert_eq!(
            before.outcome.commitment.charge_after,
            Some(at("2026-11-07 12:00"))
        );
        assert_eq!(before.due, [Due::Invoice]);

        let after = run(
            &suspended,
            &withdrawn_at("2026-11-07 12:00"),
            "2026-11-08 10:00",
        );
        assert_eq!(after.outcome.commitment.status, Status::Pending);
        assert_eq!(after.due, [Due::Notice]);
    }

    #[test]
    fn a_request_and_withdrawal_between_two_runs_across_the_charge_date_need_a_new_notice() {
        // Notice 10-31 12:00, charge after 11-07 12:00; the run never saw the request.
        let c = noticed(&base());
        let across = DailyFacts {
            withdrawn_cancellations: vec![(at("2026-11-06 09:00"), at("2026-11-07 12:00"))],
            ..facts()
        };
        let day = run(&c, &across, "2026-11-07 14:00");
        let out = &day.outcome;
        assert_eq!(out.commitment.status, Status::Pending);
        assert_eq!(
            (out.commitment.notice_sent_at, out.commitment.charge_after),
            (None, None)
        );
        assert_eq!(out.commitment.notice_cycle_at, Some(at("2026-11-07 14:00")));
        assert_eq!(kinds(out), [EventKind::Resumed]);
        assert_eq!(out.events[0].from, Some(Status::NoticeSent));
        assert_eq!(out.events[0].detail["withdrawn_at"], "2026-11-07T12:00:00Z");
        assert_eq!(day.due, [Due::Notice]);

        // Withdrawn before the charge date, or asked on or after it: the fee is due.
        for (asked, withdrawn) in [
            ("2026-11-06 09:00", "2026-11-07 11:59"),
            ("2026-11-07 12:00", "2026-11-07 13:00"),
        ] {
            let facts = DailyFacts {
                withdrawn_cancellations: vec![(at(asked), at(withdrawn))],
                ..facts()
            };
            let day = run(&c, &facts, "2026-11-07 14:00");
            assert_eq!(day.outcome, unchanged(&c));
            assert_eq!(day.due, [Due::Invoice]);
        }
    }

    #[test]
    fn a_withdrawal_once_the_deadline_moved_ahead_drops_the_held_notice() {
        let c = Commitment {
            deadline_at: at("2026-11-20 10:00"),
            ..run(
                &noticed(&base()),
                &requested("2026-11-02 09:00"),
                "2026-11-02 10:00",
            )
            .outcome
            .commitment
        };
        let day = run(&c, &facts(), "2026-11-05 10:00");
        let resumed = &day.outcome.commitment;
        assert_eq!(resumed.status, Status::Pending);
        assert_eq!((resumed.notice_sent_at, resumed.charge_after), (None, None));
        assert_eq!(day.outcome.events[0].detail["resumes_at"], "countdown");
    }

    // daily: tolling

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

    // daily: the deadline

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
    fn the_notice_cycle_starts_once_and_the_sent_notice_keeps_it() {
        let first = run(&base(), &facts(), "2026-10-31 11:00");
        assert_eq!(first.due, [Due::Notice]);
        let due = first.outcome.commitment;
        assert_eq!(due.notice_cycle_at, Some(at("2026-10-31 11:00")));
        // No email went out: the next day is the same cycle.
        let again = run(&due, &facts(), "2026-11-01 11:00");
        assert_eq!(again.outcome, unchanged(&due));
        assert_eq!(again.due, [Due::Notice]);
        let sent = notice_sent(&due, at("2026-11-01 11:05")).commitment;
        assert_eq!(sent.notice_cycle_at, Some(at("2026-10-31 11:00")));
        assert_eq!(sent.charge_after, Some(at("2026-11-08 11:05")));
    }

    #[test]
    fn a_late_withdrawn_suspension_resumes_the_notice_without_a_new_email() {
        let due = run(&base(), &facts(), "2026-10-31 11:00")
            .outcome
            .commitment;
        let sent = notice_sent(&due, at("2026-10-31 11:05")).commitment;
        let request = DailyFacts {
            cancellation_requested_at: Some(at("2026-11-02 09:00")),
            ..facts()
        };
        let suspended = run(&sent, &request, "2026-11-02 11:00").outcome.commitment;
        assert_eq!(suspended.status, Status::Suspended);
        assert_eq!(suspended.notice_cycle_at, None);

        let resumed = run(&suspended, &facts(), "2026-11-04 11:00");
        assert_eq!(kinds(&resumed.outcome), [EventKind::Resumed]);
        assert_eq!(
            resumed.outcome.events[0].detail["charge_after"],
            "2026-11-07T11:05:00Z"
        );
        assert!(resumed.due.is_empty());
        let c = &resumed.outcome.commitment;
        assert_eq!(c.status, Status::NoticeSent);
        assert_eq!(c.charge_after, Some(at("2026-11-07 11:05")));
        assert_eq!(c.notice_cycle_at, None);
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
    fn charging_off_at_the_deadline_skips_the_fee_for_good() {
        let off = DailyFacts {
            charging_enabled: false,
            ..facts()
        };
        let day = run(&base(), &off, "2026-10-31 10:00");
        let c = &day.outcome.commitment;
        assert_eq!(c.status, Status::FeeSkipped);
        assert_eq!(kinds(&day.outcome), [EventKind::FeeSkipped]);
        assert_eq!(day.outcome.events[0].detail["reason"], "charging_off");
        assert!(day.due.is_empty());

        let later = run(c, &facts(), "2026-11-10 10:00");
        assert_eq!(later.outcome, unchanged(c));
        assert!(later.due.is_empty());
    }

    #[test]
    fn no_consent_at_the_deadline_skips_the_fee() {
        let no_consent = DailyFacts {
            consent_on_file: false,
            ..facts()
        };
        let day = run(&base(), &no_consent, "2026-10-31 10:00");
        assert_eq!(day.outcome.commitment.status, Status::FeeSkipped);
        assert_eq!(day.outcome.events[0].detail["reason"], "no_consent");
        assert!(day.due.is_empty());
    }

    // daily: the charge date

    #[test]
    fn invoice_falls_due_on_the_charge_date_not_before() {
        let c = noticed(&base());
        let early = run(&c, &facts(), "2026-11-07 11:59");
        assert_eq!(early.outcome, unchanged(&c));
        assert!(early.due.is_empty());
        let day = run(&c, &facts(), "2026-11-07 12:00");
        assert_eq!(day.outcome, unchanged(&c));
        assert_eq!(day.due, [Due::Invoice]);
    }

    #[test]
    fn invoice_waits_for_a_call_inside_its_no_show_window() {
        // Ends 11:00, before the 12:00 charge date.
        let c = book(&noticed(&base()), call("2026-11-07 10:00"));
        let day = run(&c, &facts(), "2026-11-07 12:00");
        assert_eq!(day.outcome, unchanged(&c));
        assert!(day.due.is_empty());
        let done = run(&c, &facts(), "2026-11-09 11:00");
        assert_eq!(done.outcome.commitment.status, Status::Completed);
    }

    #[test]
    fn call_after_the_charge_date_does_not_hold_the_invoice() {
        let c = book(&noticed(&base()), call("2026-11-07 12:00"));
        assert_eq!(due_at(&c, "2026-11-07 12:00"), [Due::Invoice]);
    }

    #[test]
    fn charging_off_or_no_consent_at_the_charge_date_skips_the_fee() {
        let cases = [
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
        ];
        for (case, reason) in cases {
            let day = run(&noticed(&base()), &case, "2026-11-07 12:00");
            assert_eq!(day.outcome.commitment.status, Status::FeeSkipped);
            assert_eq!(day.outcome.events[0].from, Some(Status::NoticeSent));
            assert_eq!(day.outcome.events[0].detail["reason"], reason);
            assert!(day.due.is_empty());
        }
    }

    // daily: reminders

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

    // notice_sent

    #[test]
    fn notice_sets_the_charge_date_seven_days_out() {
        for c in [base(), booked("2026-11-03 15:00")] {
            let out = notice_sent(&c, at("2026-10-31 12:00"));
            let sent = &out.commitment;
            assert_eq!(sent.status, Status::NoticeSent);
            assert_eq!(sent.notice_sent_at, Some(at("2026-10-31 12:00")));
            assert_eq!(sent.charge_after, Some(at("2026-11-07 12:00")));
            assert_eq!(sent.booking, c.booking);
            assert_eq!(kinds(&out), [EventKind::NoticeSent]);
            assert_eq!(out.events[0].from, Some(c.status));
            assert_eq!(out.events[0].detail["charge_after"], "2026-11-07T12:00:00Z");
            assert_eq!(out.events[0].detail["fee_cents"], 60_000);
        }
    }

    #[test]
    fn notice_is_ignored_outside_pending_and_booked() {
        let again = noticed(&base());
        assert_eq!(
            notice_sent(&again, at("2026-11-01 12:00")),
            unchanged(&again)
        );
        for status in [Status::Suspended, Status::FeeInvoiced, Status::Completed] {
            let c = with_status(status);
            assert_eq!(notice_sent(&c, at("2026-11-01 12:00")), unchanged(&c));
        }
    }

    // invoice_created, invoice_event

    #[test]
    fn invoice_is_recorded_once_and_only_from_the_notice_period() {
        let out = invoice_created(&noticed(&base()), "in_1", at("2026-11-07 12:00"));
        let c = &out.commitment;
        assert_eq!(c.status, Status::FeeInvoiced);
        assert_eq!(c.stripe_invoice_id.as_deref(), Some("in_1"));
        assert_eq!(kinds(&out), [EventKind::Invoiced]);
        assert_eq!(out.events[0].detail["invoice_id"], "in_1");
        let now = at("2026-11-07 12:05");
        assert_eq!(invoice_created(c, "in_1", now), unchanged(c));
        assert_eq!(invoice_created(c, "in_2", now), unchanged(c));
        for status in [
            Status::Pending,
            Status::Booked,
            Status::Suspended,
            Status::Waived,
        ] {
            let other = with_status(status);
            assert_eq!(invoice_created(&other, "in_1", now), unchanged(&other));
        }
    }

    #[test]
    fn invoice_events_need_the_stored_invoice_id() {
        let c = invoiced();
        let now = at("2026-11-08 10:00");
        for event in [
            InvoiceEvent::Paid,
            InvoiceEvent::Failed,
            InvoiceEvent::Voided,
        ] {
            assert_eq!(invoice_event(&c, "in_other", event, now), unchanged(&c));
        }
    }

    #[test]
    fn failed_then_paid_ends_paid() {
        let now = at("2026-11-08 10:00");
        let failed = invoice_event(&invoiced(), "in_1", InvoiceEvent::Failed, now).commitment;
        assert_eq!(failed.status, Status::FeeFailed);
        let repeat = invoice_event(&failed, "in_1", InvoiceEvent::Failed, now);
        assert_eq!(repeat, unchanged(&failed));
        let paid = invoice_event(&failed, "in_1", InvoiceEvent::Paid, now);
        assert_eq!(paid.commitment.status, Status::FeePaid);
        assert_eq!(kinds(&paid), [EventKind::FeePaid]);
        assert_eq!(paid.events[0].from, Some(Status::FeeFailed));
    }

    #[test]
    fn paid_fee_is_never_undone() {
        let now = at("2026-11-08 10:00");
        let paid = invoice_event(&invoiced(), "in_1", InvoiceEvent::Paid, now).commitment;
        assert_eq!(paid.status, Status::FeePaid);
        for event in [
            InvoiceEvent::Paid,
            InvoiceEvent::Failed,
            InvoiceEvent::Voided,
        ] {
            assert_eq!(invoice_event(&paid, "in_1", event, now), unchanged(&paid));
        }
    }

    #[test]
    fn voided_invoice_waives_the_fee() {
        let now = at("2026-11-08 10:00");
        let failed = invoice_event(&invoiced(), "in_1", InvoiceEvent::Failed, now).commitment;
        for c in [invoiced(), failed] {
            let out = invoice_event(&c, "in_1", InvoiceEvent::Voided, now);
            assert_eq!(out.commitment.status, Status::Waived);
            assert_eq!(kinds(&out), [EventKind::Waived]);
            assert_eq!(out.events[0].detail["by"], "stripe_void");
            let repeat = invoice_event(&out.commitment, "in_1", InvoiceEvent::Voided, now);
            assert_eq!(repeat, unchanged(&out.commitment));
        }
    }

    // superuser_action

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
    fn extension_in_the_notice_period_returns_to_pending_and_withdraws_the_notice() {
        let c = Commitment {
            notice_cycle_at: Some(at("2026-10-31 11:00")),
            ..noticed(&base())
        };
        let out = superuser_action(&c, Action::Extend, ACTOR, at("2026-11-02 10:00")).unwrap();
        let extended = &out.commitment;
        assert_eq!(extended.status, Status::Pending);
        assert_eq!(extended.deadline_at, at("2026-11-14 11:00"));
        assert_eq!(extended.notice_sent_at, None);
        assert_eq!(extended.charge_after, None);
        assert_eq!(extended.notice_cycle_at, None);
        assert_eq!(out.events[0].from, Some(Status::NoticeSent));
        assert_eq!(out.events[0].to, Status::Pending);
        assert_eq!(out.events[0].detail["notice_withdrawn"], true);
    }

    #[test]
    fn extension_refused_when_the_deadline_would_still_be_past() {
        let now = at("2026-10-31 12:00");
        let past = Commitment {
            deadline_at: at("2026-10-17 12:00"),
            ..noticed(&base())
        };
        let refused = superuser_action(&past, Action::Extend, ACTOR, now);
        assert_eq!(refused, Err(Refusal::ExtensionStillPast));
        let just_ahead = Commitment {
            deadline_at: at("2026-10-17 12:01"),
            ..noticed(&base())
        };
        let out = superuser_action(&just_ahead, Action::Extend, ACTOR, now).unwrap();
        assert_eq!(out.commitment.status, Status::Pending);
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
    fn superuser_no_show_in_the_notice_period_keeps_the_notice() {
        let c = book(&noticed(&base()), call("2026-11-03 15:00"));
        let out = superuser_action(&c, Action::NoShow, ACTOR, at("2026-11-03 16:00")).unwrap();
        assert_eq!(out.commitment.status, Status::NoticeSent);
        assert_eq!(out.commitment.booking, None);
        assert_eq!(out.commitment.charge_after, Some(at("2026-11-07 12:00")));
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
    fn every_action_refused_on_settled_and_invoiced_commitments() {
        let now = at("2026-11-10 10:00");
        for action in [
            Action::Extend,
            Action::Waive,
            Action::Complete,
            Action::NoShow,
        ] {
            for (statuses, refusal) in [
                (SETTLED.as_slice(), Refusal::Final),
                (INVOICED.as_slice(), Refusal::AlreadyInvoiced),
            ] {
                for status in statuses {
                    let c = Commitment {
                        status: *status,
                        booking: Some(call("2026-11-10 09:00")),
                        ..base()
                    };
                    assert_eq!(superuser_action(&c, action, ACTOR, now), Err(refusal));
                }
            }
        }
    }

    // Local days

    fn secs(text: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").unwrap()
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

    // invoice_withdrawn

    fn charge_facts() -> ChargeFacts {
        ChargeFacts {
            charging_enabled: true,
            consent_on_file: true,
            subscription_ended: false,
            cancellation_requested_at: None,
        }
    }

    #[test]
    fn the_fee_is_withdrawn_for_an_end_a_timely_request_charging_off_or_no_consent() {
        // Charge after 11-07 12:00.
        let c = invoiced();
        assert_eq!(withdrawal(&c, &charge_facts()), None);
        let late = ChargeFacts {
            cancellation_requested_at: Some(at("2026-11-07 12:00")),
            ..charge_facts()
        };
        assert_eq!(withdrawal(&c, &late), None);
        let timely = ChargeFacts {
            cancellation_requested_at: Some(at("2026-11-07 11:59")),
            charging_enabled: false,
            ..charge_facts()
        };
        assert_eq!(
            withdrawal(&c, &timely),
            Some(Withdrawal::CancellationRequested(at("2026-11-07 11:59")))
        );
        let ended = ChargeFacts {
            subscription_ended: true,
            ..timely
        };
        assert_eq!(withdrawal(&c, &ended), Some(Withdrawal::SubscriptionEnded));
        let off = ChargeFacts {
            charging_enabled: false,
            consent_on_file: false,
            ..charge_facts()
        };
        assert_eq!(withdrawal(&c, &off), Some(Withdrawal::ChargingOff));
        let no_consent = ChargeFacts {
            consent_on_file: false,
            ..charge_facts()
        };
        assert_eq!(withdrawal(&c, &no_consent), Some(Withdrawal::NoConsent));
    }

    #[test]
    fn a_withdrawn_invoice_skips_voids_or_suspends_and_forgets_the_invoice() {
        let now = at("2026-11-08 10:00");
        for (reason, status, kind) in [
            (
                Withdrawal::ChargingOff,
                Status::FeeSkipped,
                EventKind::FeeSkipped,
            ),
            (
                Withdrawal::NoConsent,
                Status::FeeSkipped,
                EventKind::FeeSkipped,
            ),
            (
                Withdrawal::SubscriptionEnded,
                Status::Void,
                EventKind::Voided,
            ),
        ] {
            let out = invoice_withdrawn(&invoiced(), reason, now);
            assert_eq!(out.commitment.status, status);
            assert_eq!(out.commitment.stripe_invoice_id, None);
            assert_eq!(kinds(&out), [kind]);
            assert_eq!(out.events[0].from, Some(Status::FeeInvoiced));
            assert_eq!(out.events[0].detail["reason"], reason.as_str());
            assert_eq!(out.events[0].detail["invoice_withdrawn"], "in_1");
            for event in [InvoiceEvent::Paid, InvoiceEvent::Voided] {
                let later = invoice_event(&out.commitment, "in_1", event, now);
                assert_eq!(later, unchanged(&out.commitment));
            }
        }
        for status in [Status::NoticeSent, Status::FeeFailed, Status::FeePaid] {
            let other = with_status(status);
            let out = invoice_withdrawn(&other, Withdrawal::ChargingOff, now);
            assert_eq!(out, unchanged(&other));
        }
    }

    #[test]
    fn a_request_before_the_charge_suspends_the_invoice_and_its_withdrawal_needs_a_new_notice() {
        let requested_at = at("2026-11-07 11:00");
        let out = invoice_withdrawn(
            &invoiced(),
            Withdrawal::CancellationRequested(requested_at),
            at("2026-11-07 12:30"),
        );
        let c = &out.commitment;
        assert_eq!(c.status, Status::Suspended);
        assert_eq!(c.suspended_at, Some(at("2026-11-07 12:30")));
        assert_eq!(c.stripe_invoice_id, None);
        assert_eq!(c.charge_after, Some(at("2026-11-07 12:00")));
        assert_eq!(c.notice_sent_at, Some(at("2026-10-31 12:00")));
        assert_eq!(kinds(&out), [EventKind::Suspended]);
        assert_eq!(out.events[0].detail["requested_at"], "2026-11-07T11:00:00Z");
        assert_eq!(out.events[0].detail["invoice_withdrawn"], "in_1");

        let still = run(c, &requested("2026-11-07 11:00"), "2026-11-08 10:00");
        assert_eq!(still.outcome, unchanged(c));
        // Withdrawn after the charge date: a new notice before any charge.
        let resumed = run(c, &facts(), "2026-11-09 10:00");
        assert_eq!(resumed.outcome.commitment.status, Status::Pending);
        assert_eq!(resumed.outcome.commitment.charge_after, None);
        assert_eq!(resumed.due, [Due::Notice]);
    }
}
