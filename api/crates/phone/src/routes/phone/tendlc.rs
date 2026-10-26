//! The 10DLC texting registration (`phone::tendlc`): US carriers block
//! business texts from local numbers until the company's Brand and
//! Campaign are registered. Session cookie only.
//!
//! The company's Managers (holders of the Manager position in the company
//! the request acts in; admins without it are refused) fill it in:
//!
//! - `GET  /v1/phone/admin/tendlc`         → the form (a suggested first draft before any save), status, problems, feedback
//! - `PUT  /v1/phone/admin/tendlc`         → save the form as a draft
//! - `POST /v1/phone/admin/tendlc/ready`   → mark it ready (the superusers are told)
//!
//! Only superusers send it to Telnyx (fees) and follow it:
//!
//! - `GET  /v1/superuser/tendlc`                         → every company's registration, by status
//! - `GET|PUT /v1/superuser/companies/{id}/tendlc`       → review it, edit it
//! - `GET  /v1/superuser/companies/{id}/tendlc/cost`     → what Telnyx charges for its campaign
//! - `POST /v1/superuser/companies/{id}/tendlc/send`     → register the brand, then the campaign
//! - `POST /v1/superuser/companies/{id}/tendlc/resend`   → again after a rejection (changes, appeal)
//! - `POST /v1/superuser/companies/{id}/tendlc/refresh`  → read it again from Telnyx
//! - `POST /v1/superuser/companies/{id}/tendlc/numbers`  → assign the company's numbers to the approved campaign
//!
//! Every save, send and assignment is written to `phone_audit_log` (ids,
//! statuses and counts only). Telnyx's reasons come back with phone numbers
//! masked.

use crate::auth::session::SessionUser;
use crate::auth::{CompanyManager, CurrentUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::{JsonBody, MultiQuery};
use crate::openapi::{IntoParams, ToSchema};
use crate::openapi::{OpenApiRouter, routes};
use crate::phone::company;
use crate::phone::tendlc::{
    self as form, Row, TenDlcDraft, TenDlcFeedback, TenDlcProblem, TenDlcStatus, UseCase, audit,
    locked_fields_changed, move_status,
};
use crate::phone::tendlc_registry::{self as registry, APPEAL_CHARS, RegistryError};
use crate::serde_helpers::js_date;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(get_tendlc_registration, save_tendlc_registration))
        .routes(routes!(mark_tendlc_registration_ready))
        .routes(routes!(list_tendlc_registrations))
        .routes(routes!(get_company_tendlc, save_company_tendlc))
        .routes(routes!(get_company_tendlc_cost))
        .routes(routes!(send_company_tendlc))
        .routes(routes!(resend_company_tendlc))
        .routes(routes!(refresh_company_tendlc))
        .routes(routes!(assign_company_tendlc_numbers))
}

/// What the company's Managers may do with the registration now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TenDlcActions {
    /// Change the form (`PUT`): before it is sent, and after Telnyx refused
    /// it.
    pub edit: bool,
    /// Mark it ready for a superuser (`POST .../ready`): a saved draft, or
    /// one Telnyx refused once it is fixed (`problems` must be empty).
    pub mark_ready: bool,
}

/// A company's 10DLC texting registration.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TenDlcRegistration {
    /// A registration is saved; false before the first save (`draft` is
    /// then a suggestion).
    pub saved: bool,
    /// Where it is (`draft` before the first save).
    pub status: TenDlcStatus,
    /// The form.
    pub draft: TenDlcDraft,
    /// What blocks marking it ready (or sending it); empty when complete.
    pub problems: Vec<TenDlcProblem>,
    /// Telnyx has the campaign: only its sample texts, message flow and
    /// help message can change.
    pub campaign_locked: bool,
    /// Telnyx's brand status: `OK`, `REGISTRATION_PENDING` or
    /// `REGISTRATION_FAILED`; null before sending.
    pub brand_status: Option<String>,
    /// TCR's check of the business: `SELF_DECLARED`, `VERIFIED`,
    /// `VETTED_VERIFIED` or `UNVERIFIED`; null before sending.
    pub brand_identity_status: Option<String>,
    /// Telnyx's campaign status (`TCR_PENDING` ... `MNO_PROVISIONED`, or
    /// `REFUSED` when Telnyx refused to create it); null before.
    pub campaign_status: Option<String>,
    /// Why Telnyx refused the brand (its feedback), phone numbers masked;
    /// empty when nothing is wrong.
    pub brand_feedback: Vec<TenDlcFeedback>,
    /// Why Telnyx or a carrier refused the campaign, phone numbers masked;
    /// empty when nothing is wrong.
    pub campaign_feedback: Vec<TenDlcFeedback>,
    /// What went wrong last on our side (Telnyx did not answer, ...); null
    /// when nothing.
    pub last_error: Option<String>,
    /// When the form was last saved; null before.
    #[serde(serialize_with = "js_date")]
    pub draft_saved_at: Option<DateTime<Utc>>,
    /// Who saved it last (name, else email); null when unknown.
    pub draft_saved_by_name: Option<String>,
    /// When it was last marked ready; null when never.
    #[serde(serialize_with = "js_date")]
    pub ready_at: Option<DateTime<Utc>>,
    /// Who marked it ready (name, else email); null when never.
    pub ready_by_name: Option<String>,
    /// When it was first sent to Telnyx; null before.
    #[serde(serialize_with = "js_date")]
    pub sent_at: Option<DateTime<Utc>>,
    /// When the carriers approved it; null before.
    #[serde(serialize_with = "js_date")]
    pub approved_at: Option<DateTime<Utc>>,
    /// When it was last refused; null when never.
    #[serde(serialize_with = "js_date")]
    pub rejected_at: Option<DateTime<Utc>>,
    /// What the company's Managers may do now.
    pub actions: TenDlcActions,
}

fn registration_view(row: Option<&Row>, suggestion: TenDlcDraft) -> TenDlcRegistration {
    let Some(row) = row else {
        let problems = form::problems(&suggestion);
        return TenDlcRegistration {
            saved: false,
            status: TenDlcStatus::Draft,
            draft: suggestion,
            problems,
            campaign_locked: false,
            brand_status: None,
            brand_identity_status: None,
            campaign_status: None,
            brand_feedback: Vec::new(),
            campaign_feedback: Vec::new(),
            last_error: None,
            draft_saved_at: None,
            draft_saved_by_name: None,
            ready_at: None,
            ready_by_name: None,
            sent_at: None,
            approved_at: None,
            rejected_at: None,
            actions: TenDlcActions {
                edit: true,
                mark_ready: false,
            },
        };
    };
    let draft = row.draft();
    let problems = form::problems(&draft);
    TenDlcRegistration {
        saved: true,
        status: row.status,
        problems,
        campaign_locked: row.campaign_locked(),
        brand_status: row.brand_status.clone(),
        brand_identity_status: row.brand_identity_status.clone(),
        campaign_status: row.campaign_status.clone(),
        brand_feedback: row.brand_feedback(),
        campaign_feedback: row.campaign_feedback(),
        last_error: row.last_error.clone(),
        draft_saved_at: row.draft_saved_at,
        draft_saved_by_name: row.draft_saved_by_name.clone(),
        ready_at: row.ready_at,
        ready_by_name: row.ready_by_name.clone(),
        sent_at: row.sent_at,
        approved_at: row.approved_at,
        rejected_at: row.rejected_at,
        actions: TenDlcActions {
            edit: row.status.editable(),
            mark_ready: matches!(row.status, TenDlcStatus::Draft | TenDlcStatus::Rejected),
        },
        draft,
    }
}

/// A sending failure as the API's error.
fn api_error(err: RegistryError) -> ApiError {
    match err {
        RegistryError::Invalid(message) | RegistryError::Refused(message) => {
            ApiError::BadRequest(message)
        }
        RegistryError::Conflict(message) => ApiError::Conflict(message),
        RegistryError::Unanswered(message) => ApiError::Upstream(message),
        RegistryError::NotConfigured => {
            ApiError::Unavailable("Texting registration is not set up on the server.".into())
        }
        RegistryError::Database(err) => ApiError::Database(err),
    }
}

/// Saves the form for the company (Managers and superusers): 400 for a
/// field longer than Telnyx takes or a change Telnyx no longer takes, 409
/// once it is with Telnyx. A ready registration that is no longer complete
/// goes back to draft.
async fn save(
    state: &AppState,
    company_id: i32,
    input: &TenDlcDraft,
    actor: i32,
) -> ApiResult<Row> {
    let pool = &state.pool;
    let draft = form::normalized(input);
    if let Some(message) = form::too_long(&draft) {
        return Err(ApiError::BadRequest(message));
    }
    if let Some(row) = form::load(pool, company_id).await? {
        if !row.status.editable() {
            return Err(ApiError::Conflict(
                "The registration is with Telnyx: it can't change now.".into(),
            ));
        }
        if row.campaign_locked()
            && let Some(sent) = row.sent_campaign()
            && locked_fields_changed(&sent, &draft.campaign)
        {
            return Err(ApiError::BadRequest(
                "Telnyx has the campaign: only the sample texts, the message flow and the help \
                 message can change."
                    .into(),
            ));
        }
    }
    form::save_draft(pool, company_id, &draft, actor).await?;
    let row = form::load(pool, company_id)
        .await?
        .ok_or_else(|| ApiError::Internal("the registration was not saved".into()))?;
    let problems = form::problems(&draft);
    if row.status == TenDlcStatus::Ready && !problems.is_empty() {
        move_status(pool, row.id, TenDlcStatus::Ready, TenDlcStatus::Draft, None).await?;
    }
    audit(
        pool,
        company_id,
        Some(actor),
        "tendlc_saved",
        row.id,
        &json!({ "status": row.status.as_str(), "problems": problems.len() }),
    )
    .await?;
    Ok(form::load(pool, company_id).await?.unwrap_or(row))
}

/// Get the company's texting registration (10DLC)
///
/// The registration of the company the request acts in: the form (the
/// business and how it texts), where it is, what still blocks marking it
/// ready (`problems`), Telnyx's reasons when it refused it, and what the
/// Manager may do. Before the first save, `saved` is false and `draft` is a
/// suggested first draft: the company's name in every text, sample texts
/// that end "Reply STOP to opt out", the caller's email and the company's
/// main line. Save it with `PUT`, then `POST .../ready`. The company's
/// Managers only (the Manager position in the company; admins without it
/// get 403).
#[crate::openapi::path(
    get,
    path = "/v1/phone/admin/tendlc",
    operation_id = "get_tendlc_registration",
    tag = "phone",
    responses(
        (status = 200, description = "The registration, or a suggested first draft", body = TenDlcRegistration),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a Manager of the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_tendlc_registration(
    State(state): State<AppState>,
    CompanyManager(user): CompanyManager,
) -> ApiResult<Json<TenDlcRegistration>> {
    let pool = &state.pool;
    let row = form::load(pool, user.company_id).await?;
    let suggestion = match row {
        Some(_) => TenDlcDraft::default(),
        None => {
            form::suggestion_for(pool, user.company_id, &user.email, user.name.as_deref()).await?
        }
    };
    Ok(Json(registration_view(row.as_ref(), suggestion)))
}

/// Save the company's texting registration (10DLC)
///
/// Saves the whole form as a draft of the company the request acts in (the
/// first save makes the registration) and returns it with `problems`, what
/// still blocks marking it ready. Text is trimmed, the phone saved as E.164
/// when it is a US or Canada number, keyword lists as `A,B`, empty sample
/// texts dropped. Only while it is a draft, ready or rejected (409 once it
/// is with Telnyx); a ready registration that is no longer complete goes
/// back to draft. Once Telnyx has the campaign only its sample texts,
/// message flow and help message may change. 400 for a field longer than
/// Telnyx takes or a change it no longer takes. Audited. The company's
/// Managers only.
#[crate::openapi::path(
    put,
    path = "/v1/phone/admin/tendlc",
    operation_id = "save_tendlc_registration",
    tag = "phone",
    request_body = TenDlcDraft,
    responses(
        (status = 200, description = "The registration as saved", body = TenDlcRegistration),
        (status = 400, description = "A field is too long, or a change Telnyx no longer takes", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a Manager of the company", body = ErrorBody),
        (status = 409, description = "It is with Telnyx: it can't change now", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn save_tendlc_registration(
    State(state): State<AppState>,
    CompanyManager(user): CompanyManager,
    JsonBody(input): JsonBody<TenDlcDraft>,
) -> ApiResult<Json<TenDlcRegistration>> {
    let row = save(&state, user.company_id, &input, user.id).await?;
    Ok(Json(registration_view(Some(&row), TenDlcDraft::default())))
}

/// Mark the texting registration (10DLC) ready
///
/// Says the company's registration is complete: the superusers are told
/// (bell) and one of them reviews it and sends it to Telnyx; the company
/// never sends it. Also after a rejection, once it is fixed. 400 when
/// nothing is saved yet or it is not complete (the first of `problems`);
/// 409 once it is with Telnyx. Marking a ready one again changes nothing.
/// Audited. The company's Managers only.
#[crate::openapi::path(
    post,
    path = "/v1/phone/admin/tendlc/ready",
    operation_id = "mark_tendlc_registration_ready",
    tag = "phone",
    responses(
        (status = 200, description = "The registration, ready", body = TenDlcRegistration),
        (status = 400, description = "Nothing saved yet, or not complete", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a Manager of the company", body = ErrorBody),
        (status = 409, description = "It is with Telnyx already", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn mark_tendlc_registration_ready(
    State(state): State<AppState>,
    CompanyManager(user): CompanyManager,
) -> ApiResult<Json<TenDlcRegistration>> {
    let pool = &state.pool;
    let row = form::load(pool, user.company_id)
        .await?
        .ok_or_else(|| ApiError::BadRequest("Save the registration first.".into()))?;
    if row.status != TenDlcStatus::Ready {
        if !row.status.editable() {
            return Err(ApiError::Conflict(
                "The registration is with Telnyx already.".into(),
            ));
        }
        registry::complete(&row.draft()).map_err(api_error)?;
        if move_status(pool, row.id, row.status, TenDlcStatus::Ready, Some(user.id)).await? {
            let by = user
                .name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or(user.email.as_str());
            registry::notify_ready(&state, &row, by).await?;
            audit(
                pool,
                user.company_id,
                Some(user.id),
                "tendlc_ready",
                row.id,
                &json!({ "from": row.status.as_str() }),
            )
            .await?;
        }
    }
    let row = form::load(pool, user.company_id).await?;
    Ok(Json(registration_view(
        row.as_ref(),
        TenDlcDraft::default(),
    )))
}

// ------------------------------------------------------------- superusers

/// 403 unless the caller is a superuser.
const fn superuser(user: &SessionUser) -> ApiResult<()> {
    if user.is_superuser {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}

/// 404 unless the company exists (and is not deleted).
async fn company_exists(state: &AppState, id: i32) -> ApiResult<()> {
    let found: Option<i32> =
        sqlx::query_scalar("SELECT id FROM company WHERE id = ? AND deleted_at IS NULL")
            .bind(id)
            .fetch_optional(&state.pool)
            .await?;
    found.map(|_| ()).ok_or(ApiError::NotFound)
}

/// Query of `GET /v1/superuser/tendlc`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TenDlcListQuery {
    /// Only registrations in these statuses (repeat the key for several,
    /// `?status=ready&status=rejected`); every one when left out.
    #[serde(default)]
    pub status: Vec<TenDlcStatus>,
}

/// One company's registration in the superusers' list.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TenDlcListItem {
    /// The company (`/v1/superuser/companies/{id}/tendlc`).
    pub company_id: i32,
    /// Its name.
    pub company_name: String,
    /// Where the registration is.
    pub status: TenDlcStatus,
    /// The business name on the form; may be empty.
    pub display_name: String,
    /// What blocks sending it; 0 when complete.
    pub problem_count: usize,
    /// Telnyx's brand status; null before sending.
    pub brand_status: Option<String>,
    /// Telnyx's campaign status; null before.
    pub campaign_status: Option<String>,
    /// What went wrong last; null when nothing.
    pub last_error: Option<String>,
    /// When a Manager last marked it ready; null when never.
    #[serde(serialize_with = "js_date")]
    pub ready_at: Option<DateTime<Utc>>,
    /// Who marked it ready; null when never.
    pub ready_by_name: Option<String>,
    /// When it was first sent; null before.
    #[serde(serialize_with = "js_date")]
    pub sent_at: Option<DateTime<Utc>>,
    /// When it last changed.
    #[serde(serialize_with = "js_date")]
    pub updated_at: Option<DateTime<Utc>>,
}

/// `GET /v1/superuser/tendlc`.
#[derive(Debug, Serialize, ToSchema)]
pub struct TenDlcList {
    /// Those waiting for a superuser first (ready, rejected), then the
    /// newest.
    pub registrations: Vec<TenDlcListItem>,
}

/// Telnyx's ids of the registration.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TenDlcTelnyxIds {
    /// Telnyx's brand id; null before sending.
    pub brand_id: Option<String>,
    /// TCR's brand id (`B...`); null until registered.
    pub tcr_brand_id: Option<String>,
    /// Telnyx's campaign id; null until created.
    pub campaign_id: Option<String>,
    /// TCR's campaign id (`C...`); null until registered.
    pub tcr_campaign_id: Option<String>,
}

/// One of the company's active numbers and its campaign assignment.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TenDlcNumber {
    /// The number, E.164.
    pub e164: String,
    /// Its label on the Phone page; may be empty.
    pub label: String,
    /// Telnyx's assignment status (`PENDING_ASSIGNMENT`, `ASSIGNED`,
    /// `FAILED_ASSIGNMENT`, ...); null when never assigned.
    pub status: Option<String>,
    /// Why the assignment failed (Telnyx's words, numbers masked); null
    /// when it did not.
    pub error: Option<String>,
    /// When its assignment last changed; null when never assigned.
    #[serde(serialize_with = "js_date")]
    pub updated_at: Option<DateTime<Utc>>,
}

/// What a superuser may do with the registration now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TenDlcReviewActions {
    /// Change the form (`PUT`).
    pub edit: bool,
    /// Send it to Telnyx the first time (`POST .../send`): complete, and
    /// no brand yet.
    pub send: bool,
    /// Send it again after a rejection (`POST .../resend`): complete, and
    /// Telnyx has the brand.
    pub resend: bool,
    /// Read it again from Telnyx (`POST .../refresh`).
    pub refresh: bool,
    /// Assign the company's numbers to the approved campaign
    /// (`POST .../numbers`).
    pub assign_numbers: bool,
}

/// A company's registration as a superuser reviews it.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TenDlcReview {
    /// The company.
    pub company_id: i32,
    /// Its name.
    pub company_name: String,
    /// The registration (`draft` is a suggestion while `saved` is false).
    pub registration: TenDlcRegistration,
    /// Telnyx's ids.
    pub telnyx: TenDlcTelnyxIds,
    /// The company's active numbers and their assignments.
    pub numbers: Vec<TenDlcNumber>,
    /// The company has a Telnyx setup (needed to send: its webhook token
    /// names the company in Telnyx's notifications).
    pub telnyx_set_up: bool,
    /// The company's Telnyx setup has a messaging profile (the numbers must
    /// be on it to be assigned).
    pub messaging_profile_set: bool,
    /// Who first sent it (name, else email); null before.
    pub sent_by_name: Option<String>,
    /// When it was last sent again; null when never.
    #[serde(serialize_with = "js_date")]
    pub resent_at: Option<DateTime<Utc>>,
    /// When it was last read from Telnyx; null when never.
    #[serde(serialize_with = "js_date")]
    pub refreshed_at: Option<DateTime<Utc>>,
    /// What the superuser may do now.
    pub actions: TenDlcReviewActions,
}

/// The first Manager of the company (email, name): the suggestion's contact.
async fn manager_contact(
    state: &AppState,
    company_id: i32,
) -> sqlx::Result<Option<(String, Option<String>)>> {
    sqlx::query_as(
        "SELECT u.email, u.name FROM users_positions up
           JOIN users u ON u.id = up.user_id AND u.is_deleted = 0
          WHERE up.company_id = ? AND up.position_id = 5
          ORDER BY u.id LIMIT 1",
    )
    .bind(company_id)
    .fetch_optional(&state.pool)
    .await
}

async fn review(state: &AppState, company_id: i32) -> ApiResult<TenDlcReview> {
    let pool = &state.pool;
    let row = form::load(pool, company_id).await?;
    let suggestion = match row {
        Some(_) => TenDlcDraft::default(),
        None => {
            let (email, name) = manager_contact(state, company_id)
                .await?
                .unwrap_or_default();
            form::suggestion_for(pool, company_id, &email, name.as_deref()).await?
        }
    };
    let registration = registration_view(row.as_ref(), suggestion);
    let config = company::load(pool, company_id).await?;
    let company_name: String = match &row {
        Some(row) => row.company_name.clone(),
        None => sqlx::query_scalar("SELECT name FROM company WHERE id = ?")
            .bind(company_id)
            .fetch_optional(pool)
            .await?
            .unwrap_or_default(),
    };
    let numbers = registry::numbers(pool, company_id)
        .await?
        .into_iter()
        .map(|number| TenDlcNumber {
            e164: number.e164,
            label: number.label,
            status: number.status,
            error: number.error,
            updated_at: number.updated_at,
        })
        .collect();
    let complete = registration.problems.is_empty();
    let editable = registration.status.editable();
    let has_brand = row
        .as_ref()
        .is_some_and(|row| row.telnyx_brand_id.is_some());
    let actions = TenDlcReviewActions {
        edit: editable,
        send: registration.saved && editable && complete && !has_brand,
        resend: editable && complete && has_brand,
        refresh: has_brand,
        assign_numbers: registration.status == TenDlcStatus::Approved
            && row
                .as_ref()
                .is_some_and(|row| row.telnyx_campaign_id.is_some()),
    };
    Ok(TenDlcReview {
        company_id,
        company_name,
        telnyx: TenDlcTelnyxIds {
            brand_id: row.as_ref().and_then(|row| row.telnyx_brand_id.clone()),
            tcr_brand_id: row.as_ref().and_then(|row| row.tcr_brand_id.clone()),
            campaign_id: row.as_ref().and_then(|row| row.telnyx_campaign_id.clone()),
            tcr_campaign_id: row.as_ref().and_then(|row| row.tcr_campaign_id.clone()),
        },
        numbers,
        telnyx_set_up: config.is_some(),
        messaging_profile_set: config
            .as_ref()
            .is_some_and(|config| config.messaging_profile_id.is_some()),
        sent_by_name: row.as_ref().and_then(|row| row.sent_by_name.clone()),
        resent_at: row.as_ref().and_then(|row| row.resent_at),
        refreshed_at: row.as_ref().and_then(|row| row.refreshed_at),
        registration,
        actions,
    })
}

/// List the companies' texting registrations (10DLC)
///
/// Every company's registration (deleted companies left out), those
/// waiting for a superuser first (ready, then rejected), then the newest;
/// `status` narrows it. Each row: the company, where it is, the business
/// name, how many problems block sending it, Telnyx's statuses and when it
/// was marked ready and sent. Open one with
/// `GET /v1/superuser/companies/{id}/tendlc`. Superusers only.
#[crate::openapi::path(
    get,
    path = "/v1/superuser/tendlc",
    operation_id = "list_tendlc_registrations",
    tag = "phone",
    params(TenDlcListQuery),
    responses(
        (status = 200, description = "The registrations", body = TenDlcList),
        (status = 400, description = "An unknown status", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_tendlc_registrations(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    MultiQuery(query): MultiQuery<TenDlcListQuery>,
) -> ApiResult<Json<TenDlcList>> {
    superuser(&user)?;
    let rows = form::list(&state.pool, &query.status).await?;
    Ok(Json(TenDlcList {
        registrations: rows
            .iter()
            .map(|row| {
                let draft = row.draft();
                TenDlcListItem {
                    company_id: row.company_id,
                    company_name: row.company_name.clone(),
                    status: row.status,
                    problem_count: form::problems(&draft).len(),
                    display_name: draft.brand.display_name,
                    brand_status: row.brand_status.clone(),
                    campaign_status: row.campaign_status.clone(),
                    last_error: row.last_error.clone(),
                    ready_at: row.ready_at,
                    ready_by_name: row.ready_by_name.clone(),
                    sent_at: row.sent_at,
                    updated_at: Some(row.updated_at),
                }
            })
            .collect(),
    }))
}

/// Review a company's texting registration (10DLC)
///
/// Everything about the company's registration: the form, its problems,
/// Telnyx's statuses, ids and reasons, the company's active numbers and
/// their campaign assignments, whether its Telnyx setup (needed to send)
/// and messaging profile exist, and what may be done now. Before the
/// company saved one, `registration.saved` is false and the form is a
/// suggested first draft (a superuser may fill it in with `PUT`). 404 when
/// the company does not exist. Superusers only.
#[crate::openapi::path(
    get,
    path = "/v1/superuser/companies/{id}/tendlc",
    operation_id = "get_company_tendlc",
    tag = "phone",
    params(("id" = i32, Path, description = "Company id")),
    responses(
        (status = 200, description = "The registration", body = TenDlcReview),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "No such company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_company_tendlc(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<TenDlcReview>> {
    superuser(&user)?;
    company_exists(&state, id).await?;
    Ok(Json(review(&state, id).await?))
}

/// Edit a company's texting registration (10DLC)
///
/// Saves the whole form for the company, as `PUT /v1/phone/admin/tendlc`
/// does for its Managers (same checks: 409 once it is with Telnyx, 400 for
/// a field too long or a change Telnyx no longer takes); the first save
/// makes the registration. Returns the review. Audited. Superusers only.
#[crate::openapi::path(
    put,
    path = "/v1/superuser/companies/{id}/tendlc",
    operation_id = "save_company_tendlc",
    tag = "phone",
    params(("id" = i32, Path, description = "Company id")),
    request_body = TenDlcDraft,
    responses(
        (status = 200, description = "The registration as saved", body = TenDlcReview),
        (status = 400, description = "A field is too long, or a change Telnyx no longer takes", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "No such company", body = ErrorBody),
        (status = 409, description = "It is with Telnyx: it can't change now", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn save_company_tendlc(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<TenDlcDraft>,
) -> ApiResult<Json<TenDlcReview>> {
    superuser(&user)?;
    company_exists(&state, id).await?;
    save(&state, id, &input, user.id).await?;
    Ok(Json(review(&state, id).await?))
}

/// What Telnyx charges for a campaign.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TenDlcCost {
    /// The campaign's use case (`MIXED`, ...).
    pub usecase: String,
    /// Charged when the campaign is created (Telnyx's amount, US dollars,
    /// e.g. `30.00`); null when Telnyx did not say.
    pub up_front_cost: Option<String>,
    /// Charged monthly after the first three months (US dollars); null when
    /// Telnyx did not say.
    pub monthly_cost: Option<String>,
    /// Telnyx's note on the cost; null when none.
    pub description: Option<String>,
}

/// Get what Telnyx charges for a company's campaign
///
/// Telnyx's price of a campaign of the form's use case (up front, then
/// monthly), to show before sending. The brand and each number assignment
/// cost extra (Telnyx's 10DLC price list). 400 when the form has no use
/// case yet or Telnyx refused; 502 when Telnyx did not answer; 503 when
/// Telnyx is not set up on the server. Superusers only.
#[crate::openapi::path(
    get,
    path = "/v1/superuser/companies/{id}/tendlc/cost",
    operation_id = "get_company_tendlc_cost",
    tag = "phone",
    params(("id" = i32, Path, description = "Company id")),
    responses(
        (status = 200, description = "Telnyx's price of the campaign", body = TenDlcCost),
        (status = 400, description = "No use case chosen yet, or Telnyx refused", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "No such company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "Telnyx did not answer", body = ErrorBody),
        (status = 503, description = "Telnyx is not set up on the server", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_company_tendlc_cost(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<TenDlcCost>> {
    superuser(&user)?;
    company_exists(&state, id).await?;
    let usecase: Option<UseCase> = form::load(&state.pool, id)
        .await?
        .and_then(|row| row.draft().campaign.usecase);
    let usecase = usecase.ok_or_else(|| {
        ApiError::BadRequest("Choose what the texts are about (the use case) first.".into())
    })?;
    let cost = state
        .telnyx
        .campaign_cost(usecase.as_str())
        .await
        .map_err(|err| match err {
            crate::telnyx::tendlc::TenDlcError::NotConfigured => {
                api_error(RegistryError::NotConfigured)
            }
            err if err.refused() => ApiError::BadRequest(format!(
                "Telnyx did not give the price: {}",
                err.detail().unwrap_or("it refused")
            )),
            _ => ApiError::Upstream("Telnyx did not answer. Try again in a minute.".into()),
        })?;
    Ok(Json(TenDlcCost {
        usecase: cost.usecase,
        up_front_cost: cost.up_front_cost,
        monthly_cost: cost.monthly_cost,
        description: cost.description,
    }))
}

/// Send a company's texting registration to Telnyx
///
/// The first send (Telnyx charges for the brand, and for the campaign
/// three months up front): registers the business as a brand, then, once
/// TCR registered it, the campaign (Telnyx's notifications, a refresh or
/// the phone job create it when the brand takes longer). Never a second
/// brand: a send whose answer never came looks the brand up on Telnyx
/// first. The form must be complete (the company's Managers usually mark it
/// ready first) and the company needs its Telnyx setup (Telnyx reports to
/// its webhooks). Returns the review: `sent` while the brand registers,
/// `campaign_pending`, or `rejected` with Telnyx's reasons. 400 when not
/// complete, without a Telnyx setup, or when Telnyx refused the brand
/// (nothing was made); 409 when sent already (use `.../resend`) or another
/// send runs; 502 when Telnyx did not answer about the brand; 503 when
/// Telnyx is not set up on the server. Audited. Superusers only.
#[crate::openapi::path(
    post,
    path = "/v1/superuser/companies/{id}/tendlc/send",
    operation_id = "send_company_tendlc",
    tag = "phone",
    params(("id" = i32, Path, description = "Company id")),
    responses(
        (status = 200, description = "The registration, sent", body = TenDlcReview),
        (status = 400, description = "Not complete, no Telnyx setup, or Telnyx refused the brand", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "No such company", body = ErrorBody),
        (status = 409, description = "Sent already, or another send runs", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "Telnyx did not answer", body = ErrorBody),
        (status = 503, description = "Telnyx is not set up on the server", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn send_company_tendlc(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<TenDlcReview>> {
    superuser(&user)?;
    company_exists(&state, id).await?;
    registry::send(&state, id, user.id)
        .await
        .map_err(api_error)?;
    Ok(Json(review(&state, id).await?))
}

/// Body of `POST /v1/superuser/companies/{id}/tendlc/resend`.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct TenDlcResendInput {
    /// For a campaign Telnyx or a carrier refused: what was fixed (on the
    /// website, the opt-in page, ...), for Telnyx's compliance team to look
    /// again (an appeal); up to 1000 characters. Null or empty: no appeal.
    pub appeal_reason: Option<String>,
}

/// Send a company's texting registration again after a rejection
///
/// After Telnyx or a carrier refused it, and the form was fixed: the brand
/// when it changed (or was refused); for a refused campaign its sample
/// texts, message flow and help message when they changed (the only parts
/// Telnyx takes on a registered campaign; it reviews it again), and the
/// appeal when `appealReason` is written (for a fix outside the form); a
/// new campaign when TCR refused the first one or Telnyx refused to create
/// it. Returns the review. 400 when not complete, nothing changed and no
/// appeal is written, or Telnyx refused the first change; 409 when it was
/// never sent or Telnyx is reviewing it; 502 when Telnyx did not answer;
/// 503 when Telnyx is not set up on the server. Audited. Superusers only.
#[crate::openapi::path(
    post,
    path = "/v1/superuser/companies/{id}/tendlc/resend",
    operation_id = "resend_company_tendlc",
    tag = "phone",
    params(("id" = i32, Path, description = "Company id")),
    request_body = TenDlcResendInput,
    responses(
        (status = 200, description = "The registration, sent again", body = TenDlcReview),
        (status = 400, description = "Not complete, nothing changed, an appeal too long, or Telnyx refused", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "No such company", body = ErrorBody),
        (status = 409, description = "Never sent, Telnyx is reviewing it, or another send runs", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "Telnyx did not answer", body = ErrorBody),
        (status = 503, description = "Telnyx is not set up on the server", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn resend_company_tendlc(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<TenDlcResendInput>,
) -> ApiResult<Json<TenDlcReview>> {
    superuser(&user)?;
    company_exists(&state, id).await?;
    let appeal = input
        .appeal_reason
        .as_deref()
        .map(str::trim)
        .filter(|reason| !reason.is_empty());
    if appeal.is_some_and(|reason| reason.chars().count() > APPEAL_CHARS) {
        return Err(ApiError::BadRequest(format!(
            "Use at most {APPEAL_CHARS} characters for the appeal."
        )));
    }
    registry::resend(&state, id, user.id, appeal)
        .await
        .map_err(api_error)?;
    Ok(Json(review(&state, id).await?))
}

/// Read a company's texting registration again from Telnyx
///
/// Reads the brand and campaign from Telnyx (and why they failed), and
/// moves the registration on (the company is told when it is approved,
/// rejected or suspended); a sent registration whose brand is now
/// registered gets its campaign. Returns the review. 409 before it was
/// sent; 400 or 502 when Telnyx refused or did not answer; 503 when Telnyx
/// is not set up on the server. Superusers only.
#[crate::openapi::path(
    post,
    path = "/v1/superuser/companies/{id}/tendlc/refresh",
    operation_id = "refresh_company_tendlc",
    tag = "phone",
    params(("id" = i32, Path, description = "Company id")),
    responses(
        (status = 200, description = "The registration, read again", body = TenDlcReview),
        (status = 400, description = "Telnyx refused", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "No such company", body = ErrorBody),
        (status = 409, description = "Not sent yet", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "Telnyx did not answer", body = ErrorBody),
        (status = 503, description = "Telnyx is not set up on the server", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn refresh_company_tendlc(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<TenDlcReview>> {
    superuser(&user)?;
    company_exists(&state, id).await?;
    let row = form::load(&state.pool, id)
        .await?
        .filter(|row| row.telnyx_brand_id.is_some())
        .ok_or_else(|| ApiError::Conflict("It has not been sent to Telnyx yet.".into()))?;
    registry::refresh(&state, &row, true)
        .await
        .map_err(api_error)?;
    Ok(Json(review(&state, id).await?))
}

/// `POST /v1/superuser/companies/{id}/tendlc/numbers`.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TenDlcNumbersAssigned {
    /// Numbers Telnyx took (assigned, or being assigned).
    pub assigned: usize,
    /// Numbers Telnyx refused or did not answer about (each one's `error`
    /// says why; assign again to retry them).
    pub failed: usize,
    /// Numbers assigned to the campaign already.
    pub skipped: usize,
    /// The registration with its numbers.
    pub review: TenDlcReview,
}

/// Assign a company's numbers to its approved campaign
///
/// Assigns every active number of the company that is not assigned to the
/// campaign yet (Telnyx charges for each); numbers that failed before are
/// tried again. Telnyx confirms each (its notifications, and the phone job
/// reads pending ones again). The numbers must be on the company's
/// messaging profile. 409 until the carriers approved the campaign, or
/// while another send runs; 400 when the company has no active numbers;
/// 503 when Telnyx is not set up on the server. Audited (counts only).
/// Superusers only.
#[crate::openapi::path(
    post,
    path = "/v1/superuser/companies/{id}/tendlc/numbers",
    operation_id = "assign_company_tendlc_numbers",
    tag = "phone",
    params(("id" = i32, Path, description = "Company id")),
    responses(
        (status = 200, description = "What was assigned", body = TenDlcNumbersAssigned),
        (status = 400, description = "No active numbers", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "No such company", body = ErrorBody),
        (status = 409, description = "The campaign is not approved yet, or another send runs", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 503, description = "Telnyx is not set up on the server", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn assign_company_tendlc_numbers(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<TenDlcNumbersAssigned>> {
    superuser(&user)?;
    company_exists(&state, id).await?;
    let (_, run) = registry::assign_numbers(&state, id, user.id)
        .await
        .map_err(api_error)?;
    Ok(Json(TenDlcNumbersAssigned {
        assigned: run.assigned,
        failed: run.failed,
        skipped: run.skipped,
        review: review(&state, id).await?,
    }))
}
