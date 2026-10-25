//! Onboarding with Dima: the self-serve company's onboarding call
//! commitment (Terms section 11).
//!
//! - `GET  /v1/onboarding-call`                                  → the commitment and where to book
//! - `POST /v1/onboarding-call/bookings`                         → record a call booked in the widget
//! - `GET  /v1/superuser/companies/{id}/onboarding-commitment`   → commitment, timeline, slot checks
//! - `PUT  /v1/superuser/companies/{id}/onboarding-commitment`   → extend, waive, complete, no-show
//!
//! These stay open while the company's access is paused for billing.

use crate::auth::session::{SessionUser, user_positions};
use crate::auth::{CurrentUser, EmployeeUser};
use crate::billing::{self, MANAGER_POSITION, iso};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::onboarding_call::calendly::{self, CalendlyError};
use crate::onboarding_call::commitment::{self, Action, Commitment, Outcome, Refusal, Status};
use crate::onboarding_call::store::{self, SaveError, Stored};
use crate::onboarding_call::{UTM_CAMPAIGN, utm_content};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use chrono::NaiveDateTime;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(get_onboarding_call))
        .routes(routes!(create_onboarding_call_booking))
        .routes(routes!(
            get_company_onboarding_commitment,
            update_company_onboarding_commitment
        ))
}

/// Where an onboarding commitment stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OnboardingCallStatus {
    /// No call booked yet.
    Pending,
    /// A call is booked.
    Booked,
    /// The call took place (or Granite marked it complete): no fee.
    Completed,
    /// Granite waived the fee.
    Waived,
    /// A cancellation request is scheduled; withdrawing it resumes.
    Suspended,
    /// The deadline passed and the fee notice went out; the fee is charged
    /// at `charge_after`.
    NoticeSent,
    /// The fee invoice exists in Stripe.
    FeeInvoiced,
    /// The fee was paid.
    FeePaid,
    /// The fee payment failed (Stripe retries).
    FeeFailed,
    /// The deadline passed while fee charging was off: never charged.
    FeeSkipped,
    /// The subscription ended.
    Void,
}

impl From<Status> for OnboardingCallStatus {
    fn from(status: Status) -> Self {
        match status {
            Status::Pending => Self::Pending,
            Status::Booked => Self::Booked,
            Status::Completed => Self::Completed,
            Status::Waived => Self::Waived,
            Status::Suspended => Self::Suspended,
            Status::NoticeSent => Self::NoticeSent,
            Status::FeeInvoiced => Self::FeeInvoiced,
            Status::FeePaid => Self::FeePaid,
            Status::FeeFailed => Self::FeeFailed,
            Status::FeeSkipped => Self::FeeSkipped,
            Status::Void => Self::Void,
        }
    }
}

/// The company's onboarding commitment. Times are UTC ISO strings
/// (`2026-11-20T14:00:00.000Z`).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct OnboardingCommitment {
    /// Where it stands.
    pub status: OnboardingCallStatus,
    /// The deadline as moved (tolling, the extension, a call Granite
    /// cancelled or moved).
    pub deadline_at: String,
    /// Signup + 30 days.
    pub original_deadline_at: String,
    /// Days added because no slot was bookable.
    pub tolled_days: i32,
    /// The one 14-day extension was granted.
    pub extension_used: bool,
    /// The fee in cents (2 x base plan + add-ons at signup, before tax).
    pub fee_cents: i64,
    /// Start of the booked call; null when none is booked.
    pub booked_start_at: Option<String>,
    /// End of the booked call; null when none is booked.
    pub booked_end_at: Option<String>,
    /// Calendly's page to reschedule the booked call; null when none.
    pub reschedule_url: Option<String>,
    /// When onboarding was completed; null until then.
    pub completed_at: Option<String>,
    /// When the fee notice went out; null before.
    pub notice_sent_at: Option<String>,
    /// When the fee is charged (notice + 7 days); null before the notice.
    pub charge_after: Option<String>,
}

fn opt(at: Option<NaiveDateTime>) -> Option<String> {
    at.map(iso)
}

impl From<&Commitment> for OnboardingCommitment {
    fn from(c: &Commitment) -> Self {
        let booking = c.booking.as_ref();
        Self {
            status: c.status.into(),
            deadline_at: iso(c.deadline_at),
            original_deadline_at: iso(c.original_deadline_at),
            tolled_days: c.tolled_days,
            extension_used: c.extension_used,
            fee_cents: c.fee_cents,
            booked_start_at: booking.map(|b| iso(b.start_at)),
            booked_end_at: booking.map(|b| iso(b.end_at)),
            reschedule_url: booking.and_then(|b| b.reschedule_url.clone()),
            completed_at: opt(c.completed_at),
            notice_sent_at: opt(c.notice_sent_at),
            charge_after: opt(c.charge_after),
        }
    }
}

/// Why the caller cannot book right now although they may.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BookingUnavailableReason {
    /// Booking is not set up yet: no Calendly page is configured. The
    /// deadline moves a day for every day nobody can book.
    NotConfigured,
    /// Calendly could not be reached just now: ask again in a few minutes.
    Temporary,
}

/// The onboarding call as the signed-in user sees it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct OnboardingCall {
    /// Null: the company has no commitment (a legacy company, or one moved
    /// onto a plan by staff).
    pub commitment: Option<OnboardingCommitment>,
    /// Calendly page to book on, with `name`, `email`,
    /// `utm_campaign=granite-onboarding` and `utm_content=company-<id>` in
    /// the query. Null when the caller cannot book, the status does not allow
    /// booking (only pending, booked and notice_sent do), or no page can be
    /// resolved.
    pub booking_url: Option<String>,
    /// The caller is a Manager (position 5) or an Admin of the acting
    /// company, so may book. Always false for Granite superusers (they mark
    /// the call completed on the superuser page instead).
    pub can_book: bool,
    /// No booking page could be had right now (see
    /// `booking_unavailable_reason`). Only meaningful when `can_book` and
    /// the status allows booking.
    pub booking_unavailable: bool,
    /// Why `booking_unavailable`: `not_configured` (booking is not set up
    /// yet) or `temporary` (Calendly could not be reached; try again in a
    /// few minutes). Null when booking is available or not offered.
    pub booking_unavailable_reason: Option<BookingUnavailableReason>,
}

/// A call booked in the Calendly widget.
#[derive(Debug, Deserialize, ToSchema)]
pub struct NewBooking {
    /// `payload.invitee.uri` of Calendly's `calendly.event_scheduled`
    /// message: `https://api.calendly.com/scheduled_events/<id>/invitees/<id>`.
    pub invitee_uri: String,
}

/// Managers and Admins of the acting company book; Granite superusers never
/// do (super admin positions make them admins of the company they act in).
async fn can_book(state: &AppState, user: &SessionUser) -> ApiResult<bool> {
    if user.is_superuser {
        return Ok(false);
    }
    if user.is_admin {
        return Ok(true);
    }
    let positions = user_positions(&state.pool, user.id).await?;
    Ok(positions
        .iter()
        .any(|row| row.company_id == user.company_id && row.position_id == MANAGER_POSITION))
}

/// The booking page with the caller and our tag in the query.
fn booking_link(page: &str, user: &SessionUser) -> Option<String> {
    let mut url = url::Url::parse(page).ok()?;
    url.query_pairs_mut()
        .extend_pairs(
            user.name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(|name| ("name", name)),
        )
        .append_pair("email", user.email.trim())
        .append_pair("utm_campaign", UTM_CAMPAIGN)
        .append_pair("utm_content", &utm_content(user.company_id));
    Some(url.to_string())
}

async fn call_view(
    state: &AppState,
    user: &SessionUser,
    commitment: Option<&Commitment>,
    can_book: bool,
) -> OnboardingCall {
    let bookable = commitment.is_some_and(|c| {
        matches!(
            c.status,
            Status::Pending | Status::Booked | Status::NoticeSent
        )
    });
    let mut booking_url = None;
    let mut reason = None;
    if can_book && bookable {
        match state.onboarding_call.calendly.onboarding_page().await {
            Ok(page) => {
                booking_url = page.as_deref().and_then(|page| booking_link(page, user));
                if booking_url.is_none() {
                    reason = Some(BookingUnavailableReason::NotConfigured);
                }
            }
            Err(err) => {
                tracing::warn!(code = %err.code(), "onboarding booking page not resolved");
                reason = Some(BookingUnavailableReason::Temporary);
            }
        }
    }
    OnboardingCall {
        commitment: commitment.map(OnboardingCommitment::from),
        booking_url,
        can_book,
        booking_unavailable: reason.is_some(),
        booking_unavailable_reason: reason,
    }
}

fn refused(refusal: Refusal) -> ApiError {
    ApiError::Conflict(refusal.message().into())
}

/// Applies a change to the company's commitment, read again once on a
/// version conflict.
async fn change(
    state: &AppState,
    company_id: i32,
    now: NaiveDateTime,
    apply: impl Fn(&Commitment) -> Result<Outcome, Refusal> + Send,
) -> ApiResult<Stored> {
    for _ in 0..2 {
        let stored = store::load(&state.pool, company_id)
            .await?
            .ok_or(ApiError::NotFound)?;
        let outcome = apply(&stored.commitment).map_err(refused)?;
        if !store::changes(&stored, &outcome) {
            return Ok(stored);
        }
        match store::save(&state.pool, stored.version, &outcome, now).await {
            Ok(version) => {
                return Ok(Stored {
                    commitment: outcome.commitment,
                    version,
                });
            }
            Err(SaveError::Conflict) => {}
            Err(SaveError::Database(err)) => return Err(err.into()),
        }
    }
    Err(ApiError::Conflict(
        "The onboarding call changed meanwhile. Try again.".into(),
    ))
}

/// Get the company's onboarding call
///
/// The self-serve onboarding commitment of the company the request acts in
/// (complete an onboarding call within 30 days of signup, or pay the
/// onboarding fee), with where to book. `commitment` is null for companies
/// without one. `booking_url` is set only for Managers and Admins of the
/// acting company (never for Granite superusers) while the status is
/// pending, booked or notice_sent; open it in the Calendly widget, then send
/// the booked invitee to `POST /v1/onboarding-call/bookings`. When no page
/// can be had, `booking_unavailable` is true and
/// `booking_unavailable_reason` says whether booking is not set up yet or
/// Calendly is briefly unreachable (ask again in a few minutes).
#[utoipa::path(
    get,
    path = "/v1/onboarding-call",
    operation_id = "get_onboarding_call",
    tag = "onboarding-call",
    responses(
        (status = 200, description = "The commitment and where to book", body = OnboardingCall),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_onboarding_call(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<OnboardingCall>> {
    let can_book = can_book(&state, &user).await?;
    let stored = store::load(&state.pool, user.company_id).await?;
    let commitment = stored.as_ref().map(|stored| &stored.commitment);
    Ok(Json(call_view(&state, &user, commitment, can_book).await))
}

fn calendly_failure(err: &CalendlyError) -> ApiError {
    match err {
        CalendlyError::BadUri | CalendlyError::Status(404) => {
            ApiError::BadRequest("This booking was not found in Calendly.".into())
        }
        CalendlyError::NotConfigured => {
            ApiError::Unavailable("Booking the onboarding call is not set up yet.".into())
        }
        _ => {
            tracing::warn!(code = %err.code(), "Calendly booking read failed");
            ApiError::Unavailable("Calendly could not be reached. Try again in a minute.".into())
        }
    }
}

/// Record a booked onboarding call
///
/// Call after the Calendly widget reports `calendly.event_scheduled`, with
/// its `payload.invitee.uri`. The booking is read back from Calendly: it
/// must be active and carry this company's tag (`utm_campaign` and
/// `utm_content` from `booking_url` of `GET /v1/onboarding-call`). A new
/// booking replaces the one stored (a reschedule). A call at or before a
/// recorded no-show is refused: book a new time. Managers and Admins of the
/// acting company only (Granite superusers get 403). Returns the onboarding call as
/// `GET /v1/onboarding-call` does.
#[utoipa::path(
    post,
    path = "/v1/onboarding-call/bookings",
    operation_id = "create_onboarding_call_booking",
    tag = "onboarding-call",
    request_body = NewBooking,
    responses(
        (status = 200, description = "Booking recorded", body = OnboardingCall),
        (status = 400, description = "Not a Calendly booking URI, or not this company's active onboarding call", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a Manager or Admin of the company, or is a Granite superuser", body = ErrorBody),
        (status = 404, description = "The company has no onboarding commitment", body = ErrorBody),
        (status = 409, description = "Refused: the commitment is settled or invoiced, or the call is at or before a recorded no-show (book a new time)", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 503, description = "Calendly is not set up or could not be reached", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_onboarding_call_booking(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<NewBooking>,
) -> ApiResult<Json<OnboardingCall>> {
    if !can_book(&state, &user).await? {
        return Err(ApiError::Forbidden);
    }
    let company_id = user.company_id;
    if store::load(&state.pool, company_id).await?.is_none() {
        return Err(ApiError::NotFound);
    }
    let calendly = &state.onboarding_call.calendly;
    let Some(path) = calendly.invitee_path(&input.invitee_uri) else {
        return Err(ApiError::BadRequest("Not a Calendly booking.".into()));
    };
    let invitee = calendly
        .invitee(&path)
        .await
        .map_err(|err| calendly_failure(&err))?;
    if !invitee.carries_tag(company_id) {
        return Err(ApiError::BadRequest(
            "This booking is not this company's onboarding call.".into(),
        ));
    }
    if !invitee.active {
        return Err(ApiError::BadRequest("This booking was cancelled.".into()));
    }
    let event = calendly
        .scheduled_event(&invitee.event_uri)
        .await
        .map_err(|err| calendly_failure(&err))?;
    if !event.active {
        return Err(ApiError::BadRequest("This booking was cancelled.".into()));
    }
    let now = billing::now();
    let booking = calendly::booking(&invitee, &event, now);
    let stored = change(&state, company_id, now, |c| {
        commitment::record_booking(c, booking.clone(), now)
    })
    .await?;
    Ok(Json(
        call_view(&state, &user, Some(&stored.commitment), true).await,
    ))
}

/// The commitment with what only superusers see.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SuperuserCommitment {
    #[serde(flatten)]
    pub commitment: OnboardingCommitment,
    /// The company it belongs to.
    pub company_id: i32,
    /// The superuser who marked it complete; null when the call completed it.
    pub completed_by: Option<i32>,
    /// When the last no-show was recorded; null without one.
    pub no_show_at: Option<String>,
    /// When it was suspended by a cancellation request; null when not.
    pub suspended_at: Option<String>,
    /// The fee invoice in Stripe (`in_...`); null before it exists.
    pub stripe_invoice_id: Option<String>,
    /// Signup (when the commitment was created).
    pub created_at: String,
}

/// One timeline entry: who changed what, when (the evidence if the fee is
/// disputed).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct OnboardingCommitmentEvent {
    /// Entry id (increasing).
    pub id: i64,
    /// `created`, `booked`, `rescheduled`, `booking_cancelled`, `no_show`,
    /// `tolled`, `extended`, `waived`, `completed`, `suspended`, `resumed`,
    /// `notice_sent`, `invoiced`, `fee_paid`, `fee_failed`, `fee_skipped`,
    /// `voided`.
    pub kind: String,
    /// The status before; null for `created`.
    pub from_status: Option<String>,
    /// The status after (same values as `OnboardingCallStatus`).
    pub to_status: String,
    /// The deadline after the change.
    pub deadline_at: String,
    /// The superuser who acted; null for the customer or the system.
    pub actor_user_id: Option<i32>,
    /// That superuser's name; null for the customer or the system.
    pub actor_name: Option<String>,
    /// The facts behind it (reason, old and new deadline, call times,
    /// invoice id); null when none.
    #[schema(value_type = Option<Object>)]
    pub detail: Option<serde_json::Value>,
    /// When it happened (UTC ISO).
    pub created_at: String,
}

/// One day's slot check.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AvailabilityCheck {
    /// The UTC day, `YYYY-MM-DD`.
    pub checked_on: String,
    /// `available`, `no_slots`, `not_configured` or `error`; anything but
    /// `available` moved open deadlines by a day.
    pub result: String,
    /// Bookable slots found within the next 5 business days.
    pub slots: i32,
}

/// A company's onboarding commitment for the superuser page.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SuperuserOnboardingCommitment {
    /// Null: the company has no commitment.
    pub commitment: Option<SuperuserCommitment>,
    /// The timeline, newest first (up to 100).
    pub events: Vec<OnboardingCommitmentEvent>,
    /// The slot checks of the last 14 days, newest first.
    pub availability: Vec<AvailabilityCheck>,
    /// `ONBOARDING_FEE_CHARGING=1`: a passed deadline leads to the fee.
    pub charging_enabled: bool,
}

/// What a superuser does to a commitment.
#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CommitmentAction {
    /// The one 14-day extension the customer asked for.
    Extend,
    /// No fee (a dispute, a goodwill gesture).
    Waive,
    /// Onboarding done (say, on a call outside Calendly).
    Complete,
    /// The customer missed the booked call.
    NoShow,
}

/// `PUT /v1/superuser/companies/{id}/onboarding-commitment`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CommitmentActionBody {
    /// `extend`, `waive`, `complete` or `no_show`.
    pub action: CommitmentAction,
}

const fn superuser(user: &SessionUser) -> ApiResult<()> {
    if user.is_superuser {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}

async fn superuser_view(
    state: &AppState,
    company_id: i32,
) -> ApiResult<SuperuserOnboardingCommitment> {
    let pool = &state.pool;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM company WHERE id = ?) <> 0")
            .bind(company_id)
            .fetch_one(pool)
            .await?;
    if !exists {
        return Err(ApiError::NotFound);
    }
    let stored = store::load(pool, company_id).await?;
    let events = store::timeline(pool, company_id)
        .await?
        .into_iter()
        .map(|row| OnboardingCommitmentEvent {
            id: row.id,
            kind: row.kind,
            from_status: row.from_status,
            to_status: row.to_status,
            deadline_at: iso(row.deadline_at),
            actor_user_id: row.actor_user_id,
            actor_name: row.actor_name,
            detail: row
                .detail
                .as_deref()
                .and_then(|detail| serde_json::from_str(detail).ok()),
            created_at: iso(row.created_at),
        })
        .collect();
    let availability = store::recent_availability(pool, billing::now().date())
        .await?
        .into_iter()
        .map(|day| AvailabilityCheck {
            checked_on: day.checked_on.format("%Y-%m-%d").to_string(),
            result: day.result,
            slots: day.slots,
        })
        .collect();
    Ok(SuperuserOnboardingCommitment {
        commitment: stored.map(|Stored { commitment: c, .. }| SuperuserCommitment {
            commitment: OnboardingCommitment::from(&c),
            company_id: c.company_id,
            completed_by: c.completed_by,
            no_show_at: opt(c.no_show_at),
            suspended_at: opt(c.suspended_at),
            stripe_invoice_id: c.stripe_invoice_id,
            created_at: iso(c.created_at),
        }),
        events,
        availability,
        charging_enabled: state.onboarding_call.fee_charging,
    })
}

/// Get a company's onboarding commitment
///
/// The self-serve onboarding commitment of any company, with its full
/// timeline (who changed what, when; newest first, up to 100), the daily
/// slot checks of the last 14 days and whether fee charging is on.
/// `commitment` is null for a company without one. Change it with
/// `PUT /v1/superuser/companies/{id}/onboarding-commitment`. Superusers only.
#[utoipa::path(
    get,
    path = "/v1/superuser/companies/{id}/onboarding-commitment",
    operation_id = "get_company_onboarding_commitment",
    tag = "superuser",
    params(("id" = i32, Path, description = "Company id")),
    responses(
        (status = 200, description = "The commitment, timeline and slot checks", body = SuperuserOnboardingCommitment),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "No such company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_company_onboarding_commitment(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<SuperuserOnboardingCommitment>> {
    superuser(&user)?;
    Ok(Json(superuser_view(&state, id).await?))
}

/// Extend, waive, complete or mark a no-show on a company's onboarding commitment
///
/// `extend`: the one 14-day extension (refused when used, or when the
/// deadline would still be in the past). `waive`: no fee. `complete`:
/// onboarding done. `no_show`: the customer missed the booked call (only
/// once it started and within 48 hours after it ended). All are refused
/// once the commitment is settled or the fee is invoiced (void or refund in
/// Stripe instead). Each is stored on the timeline with the acting
/// superuser and the time. Returns the commitment as
/// `GET /v1/superuser/companies/{id}/onboarding-commitment` does.
#[utoipa::path(
    put,
    path = "/v1/superuser/companies/{id}/onboarding-commitment",
    operation_id = "update_company_onboarding_commitment",
    tag = "superuser",
    params(("id" = i32, Path, description = "Company id")),
    request_body = CommitmentActionBody,
    responses(
        (status = 200, description = "Done", body = SuperuserOnboardingCommitment),
        (status = 400, description = "Unknown action", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "The company has no onboarding commitment", body = ErrorBody),
        (status = 409, description = "Refused: already extended or invoiced, settled, not booked, or the extension would leave the deadline in the past", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_company_onboarding_commitment(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<CommitmentActionBody>,
) -> ApiResult<Json<SuperuserOnboardingCommitment>> {
    superuser(&user)?;
    let action = match input.action {
        CommitmentAction::Extend => Action::Extend,
        CommitmentAction::Waive => Action::Waive,
        CommitmentAction::Complete => Action::Complete,
        CommitmentAction::NoShow => Action::NoShow,
    };
    let now = billing::now();
    change(&state, id, now, |c| {
        commitment::superuser_action(c, action, user.id, now)
    })
    .await?;
    Ok(Json(superuser_view(&state, id).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(name: Option<&str>) -> SessionUser {
        SessionUser {
            id: 1,
            email: " owen+test@example.test ".into(),
            name: name.map(str::to_string),
            phone_number: None,
            is_employee: true,
            is_admin: false,
            is_superuser: false,
            company_id: 20400,
            pined_bar: false,
            cloudtalk_agent_id: None,
            cloudtalk_phone_number: None,
            ringcentral_extension_id: None,
            ringcentral_phone_number: None,
            is_signature_available: false,
            plan: crate::entitlements::Entitlements::default(),
        }
    }

    #[test]
    fn the_booking_link_carries_the_caller_and_our_tag_encoded() {
        assert_eq!(
            booking_link(
                "https://calendly.com/dima/onboarding",
                &user(Some("Owen & Co"))
            )
            .as_deref(),
            Some(
                "https://calendly.com/dima/onboarding?name=Owen+%26+Co&email=owen%2Btest%40example.test&utm_campaign=granite-onboarding&utm_content=company-20400"
            )
        );
        assert_eq!(
            booking_link("https://calendly.com/dima", &user(Some("  "))).as_deref(),
            Some(
                "https://calendly.com/dima?email=owen%2Btest%40example.test&utm_campaign=granite-onboarding&utm_content=company-20400"
            )
        );
    }
}
