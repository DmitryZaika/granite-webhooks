//! Guided tours: the first-run journey, feature tours and their points.
//!
//! - `GET  /v1/me/onboarding`        → the saved progress
//! - `POST /v1/me/onboarding/events` → apply progress events
//! - `POST /v1/me/onboarding/reset`  → restart the first-run journey
//! - `GET  /v1/me/onboarding/setup`  → the dashboard widget: profile, setup
//!   checklist, and credit for setup already done
//! - `POST /v1/me/onboarding/claim`  → complete a setup task whose work is done
//! - `GET  /v1/onboarding/team`      → the company's tours progress (admins;
//!   see [`team`])
//!
//! Replaces the Remix `api.onboarding.events`, `api.onboarding.reset` and
//! `api.employee.onboarding-setup` routes. Progress is the user's HOME
//! company's (`users.company_id`), never a super admin's switched company.

pub mod manifest;
pub mod store;
pub(crate) mod team;

use crate::auth::ShopOrEmployeeUser;
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use store::{Actor, Applied, OnboardingEvent, OnboardingRecord, OnboardingState};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(progress))
        .routes(routes!(events))
        .routes(routes!(reset))
        .routes(routes!(setup))
        .routes(routes!(claim))
        .routes(routes!(team::team))
}

/// Most events one request may carry.
const MAX_EVENTS: usize = 200;

const SALES_REP: i32 = 1;
const SHOP_WORKER: i32 = 4;
const SHOP_MANAGER: i32 = 3;
const MANAGER: i32 = 5;
const INSTALLER: i32 = 6;

async fn home_actor(state: &AppState, user_id: i32) -> ApiResult<Actor> {
    store::actor(&state.pool, user_id)
        .await?
        .ok_or(ApiError::Unauthorized)
}

/// My onboarding progress
///
/// The first-run journey, feature tours and points saved for the signed-in
/// user (their home company's journey). `has_row` is false before any.
#[utoipa::path(
    get,
    path = "/v1/me/onboarding",
    operation_id = "get_my_onboarding",
    tag = "me",
    responses(
        (status = 200, description = "Progress", body = OnboardingRecord),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not an employee or shop user", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn progress(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
) -> ApiResult<Json<OnboardingRecord>> {
    let actor = home_actor(&state, user.id).await?;
    Ok(Json(store::load_record(&state.pool, actor.user_id).await?))
}

/// Body of `POST /v1/me/onboarding/events`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct EventsInput {
    /// 1 to 200 events, oldest first. Each has a `type` and an ISO 8601 `at`:
    /// `journey.position` (`moduleId`, `stepKey`), `journey.module_step`,
    /// `journey.module_completed`, `journey.module_reopened` (`moduleId`),
    /// `journey.dismissed`, `journey.resumed`, `journey.completed`,
    /// `journey.feedback_dismissed`, `task.position` and `task.closed`
    /// (`taskId`, `version`, `stepKey`), `task.step_done` (`taskId`,
    /// `version`, `steps`: step key → `action` | `manual`), `task.completed`
    /// (`taskId`, `version`).
    #[schema(value_type = Vec<Object>)]
    pub events: Vec<OnboardingEvent>,
}

/// The progress after events.
#[derive(Debug, Serialize, ToSchema)]
pub struct AppliedProgress {
    /// The saved state.
    pub state: OnboardingState,
    /// When the first-run journey was completed.
    pub completed_at: Option<String>,
    /// The home company's guided tours are on (off: task events were dropped).
    pub feature_tasks_enabled: bool,
}

/// Save onboarding progress
///
/// Applies the events in one write. Replays are harmless; events dated
/// before the user's last reset are dropped; task events are dropped while
/// the home company's guided tours are off, for retired tours, and for setup
/// tasks only the server may complete. Points come from the tour registry,
/// never from the request.
#[utoipa::path(
    post,
    path = "/v1/me/onboarding/events",
    operation_id = "save_my_onboarding_events",
    tag = "me",
    request_body = EventsInput,
    responses(
        (status = 200, description = "Saved", body = AppliedProgress),
        (status = 400, description = "Malformed events", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not an employee or shop user", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn events(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    JsonBody(input): JsonBody<EventsInput>,
) -> ApiResult<Json<AppliedProgress>> {
    if input.events.is_empty() || input.events.len() > MAX_EVENTS {
        return Err(ApiError::BadRequest("Invalid request".into()));
    }
    if let Some(problem) = input.events.iter().find_map(OnboardingEvent::problem) {
        return Err(ApiError::BadRequest(problem.into()));
    }
    let actor = home_actor(&state, user.id).await?;
    let Applied::Done {
        feature_tasks_enabled,
    } = store::apply_events(&state.pool, actor, &input.events, false).await?
    else {
        return Err(ApiError::Unauthorized);
    };
    let record = store::load_record(&state.pool, actor.user_id).await?;
    Ok(Json(AppliedProgress {
        state: record.state,
        completed_at: record.completed_at,
        feature_tasks_enabled,
    }))
}

/// Restart the first-run journey
///
/// Clears the old tours' modules and position; feature tours and points
/// stay. Events the device still holds from before are dropped.
#[utoipa::path(
    post,
    path = "/v1/me/onboarding/reset",
    operation_id = "reset_my_onboarding",
    tag = "me",
    responses(
        (status = 204, description = "Restarted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not an employee or shop user", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn reset(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
) -> ApiResult<StatusCode> {
    let actor = home_actor(&state, user.id).await?;
    if !store::reset_journey(&state.pool, actor).await? {
        return Err(ApiError::Unauthorized);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// What the onboarding widget knows about the user.
#[derive(Debug, Serialize, ToSchema)]
pub struct OnboardingProfile {
    /// The user.
    pub user_id: i32,
    /// Admin or super user.
    pub is_admin: bool,
    /// May manage users (super user, or Manager).
    pub can_manage_users: bool,
    /// The company has CloudTalk.
    pub has_cloudtalk_api: bool,
    /// The company has RingCentral.
    pub has_ringcentral_api: bool,
    /// The company receives email.
    pub has_inbound_email: bool,
    /// The user has a CloudTalk agent.
    pub has_cloudtalk_agent: bool,
    /// Holds the Shop Worker position.
    pub is_shop_worker: bool,
    /// Holds the Shop Manager position.
    pub is_shop_manager: bool,
    /// When the first-run journey was completed.
    pub onboarding_completed_at: Option<String>,
    /// The saved progress; null before any.
    pub saved_progress: Option<OnboardingState>,
}

/// The setup checklist (`EmployeeSetupChecklistStatus`).
#[derive(Debug, Default, Serialize, ToSchema)]
pub struct SetupStatus {
    /// An email sender name is set.
    pub has_email_name: bool,
    /// An email signature is set.
    pub has_email_signature: bool,
    /// Linked to both Telegram bots.
    pub has_telegram: bool,
    /// A monthly sales goal is set.
    pub has_sales_goal: bool,
    /// Admin checklist rows apply.
    pub is_admin: bool,
    /// Holds the Sales Rep position.
    pub is_sales_rep: bool,
    /// The company has a sales rep.
    pub has_sales_rep: bool,
    /// The company has an installer.
    pub has_installer: bool,
    /// A walk-in has checked in.
    pub has_walk_in_login: bool,
    /// The company has a logo.
    pub has_logo: bool,
    /// The company sales tax is set.
    pub has_sales_tax: bool,
    /// QuickBooks is connected.
    pub has_quick_books: bool,
    /// QuickBooks is switched on.
    pub quickbooks_feature_enabled: bool,
    /// A supplier price list is processed.
    pub has_supplier_price_lists: bool,
}

/// Setup tasks credited by this request.
#[derive(Debug, Serialize, ToSchema)]
pub struct Credit {
    /// The tasks now completed.
    pub task_ids: Vec<String>,
    /// The saved state after the credit.
    pub state: OnboardingState,
}

/// `GET /v1/me/onboarding/setup`.
#[derive(Debug, Serialize, ToSchema)]
pub struct SetupPage {
    /// The onboarding profile.
    pub profile: OnboardingProfile,
    /// The setup checklist.
    pub setup: SetupStatus,
    /// Null unless something was credited just now.
    pub credit: Option<Credit>,
}

/// The onboarding sample supplier, left out of the price list check.
const SAMPLE_SUPPLIER: &str = "Onboarding Sample Supplier";

async fn setup_status(
    state: &AppState,
    user: &SessionUser,
    as_admin: bool,
) -> ApiResult<SetupStatus> {
    let pool = &state.pool;
    let profile: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT email_name, email_signature FROM users WHERE id = ? AND is_deleted = 0",
    )
    .bind(user.id)
    .fetch_optional(pool)
    .await?;
    let (name, signature) = profile.unwrap_or_default();
    let telegram = crate::routes::me::handlers::telegram_status(state, user.id).await?;
    let goal = crate::routes::me::queries::sales_goal(pool, user.id).await?;
    let is_sales_rep: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM users_positions WHERE user_id = ? AND company_id = ? AND position_id = ? LIMIT 1",
    )
    .bind(user.id)
    .bind(user.company_id)
    .bind(SALES_REP)
    .fetch_optional(pool)
    .await?;
    let mut status = SetupStatus {
        has_email_name: name.is_some_and(|name| !name.trim().is_empty()),
        has_email_signature: signature.is_some_and(|signature| {
            let signature = signature.trim();
            !signature.is_empty()
                && (!signature.contains('<')
                    || !crate::routes::teach_mode::text_from_html(signature).is_empty())
        }),
        has_telegram: telegram.leads_linked && telegram.notifications_linked,
        has_sales_goal: goal.monthly_sales_goal.is_some(),
        is_sales_rep: is_sales_rep.is_some(),
        quickbooks_feature_enabled: true,
        ..SetupStatus::default()
    };
    if as_admin {
        let company = user.company_id;
        let features = crate::routes::company::queries::company_features(pool, company).await?;
        let (logo, taxes): (Option<String>, bool) =
            sqlx::query_as("SELECT logo_url, state_taxes IS NOT NULL FROM company WHERE id = ?")
                .bind(company)
                .fetch_optional(pool)
                .await?
                .unwrap_or((None, false));
        let positions: Vec<i32> = sqlx::query_scalar(
            "SELECT DISTINCT up.position_id FROM users u JOIN users_positions up ON up.user_id = u.id
              WHERE u.is_deleted = 0 AND up.company_id = ? AND up.position_id IN (?, ?)",
        )
        .bind(company)
        .bind(SALES_REP)
        .bind(INSTALLER)
        .fetch_all(pool)
        .await?;
        let walk_in: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM customers WHERE company_id = ? AND source = 'check-in' AND deleted_at IS NULL LIMIT 1",
        )
        .bind(company)
        .fetch_optional(pool)
        .await?;
        let price_list: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM supplier_files sf JOIN suppliers s ON s.id = sf.supplier_id
              WHERE s.company_id = ? AND sf.status = 'ready' AND s.supplier_name <> ? LIMIT 1",
        )
        .bind(company)
        .bind(SAMPLE_SUPPLIER)
        .fetch_optional(pool)
        .await?;
        status.is_admin = true;
        status.has_sales_rep = positions.contains(&SALES_REP);
        status.has_installer = positions.contains(&INSTALLER);
        status.has_walk_in_login = walk_in.is_some();
        status.has_logo = logo.is_some_and(|logo| !logo.trim().is_empty());
        status.has_sales_tax = taxes;
        status.has_quick_books = features.quickbooks_connected;
        status.has_supplier_price_lists = price_list.is_some();
        status.quickbooks_feature_enabled = features.quickbooks_enabled;
    }
    Ok(status)
}

/// The setup tasks whose work the status shows done.
fn setup_done(setup: &SetupStatus) -> Vec<&'static str> {
    let mut done = Vec::new();
    if setup.has_email_name {
        done.push("setup-email-name");
    }
    if setup.has_email_signature {
        done.push("setup-email-signature");
    }
    if setup.has_telegram {
        done.push("setup-telegram");
    }
    done
}

/// How a credit went.
pub enum CreditOutcome {
    /// No such user, or the home company's guided tours are off.
    Unavailable,
    /// Every task was already completed.
    NothingPending,
    Applied {
        credited: Vec<String>,
        state: OnboardingState,
        enabled: bool,
    },
}

/// Completes the done setup tasks not completed yet, with their points.
pub async fn credit_done(
    state: &AppState,
    user_id: i32,
    done: &[&str],
) -> ApiResult<CreditOutcome> {
    let pool = &state.pool;
    let Some(actor) = store::actor(pool, user_id).await? else {
        return Ok(CreditOutcome::Unavailable);
    };
    if !crate::routes::company::queries::company_features(pool, actor.company_id)
        .await?
        .onboarding
    {
        return Ok(CreditOutcome::Unavailable);
    }
    let record = store::load_record(pool, user_id).await?;
    let pending: Vec<&str> = done
        .iter()
        .copied()
        .filter(|task| {
            record
                .state
                .tasks
                .get(*task)
                .is_none_or(|progress| progress.status != "completed")
        })
        .collect();
    if pending.is_empty() {
        return Ok(CreditOutcome::NothingPending);
    }
    let at = chrono::Utc::now().to_rfc3339();
    let events: Vec<OnboardingEvent> = pending
        .iter()
        .filter_map(|task_id| {
            manifest::task(task_id).map(|task| OnboardingEvent::TaskCompleted {
                at: at.clone(),
                task_id: (*task_id).to_string(),
                version: task.version,
            })
        })
        .collect();
    let Applied::Done {
        feature_tasks_enabled,
    } = store::apply_events(pool, actor, &events, true).await?
    else {
        return Ok(CreditOutcome::Unavailable);
    };
    let record = store::load_record(pool, user_id).await?;
    let credited = pending
        .into_iter()
        .filter(|task| {
            record
                .state
                .tasks
                .get(*task)
                .is_some_and(|progress| progress.status == "completed")
        })
        .map(str::to_string)
        .collect();
    Ok(CreditOutcome::Applied {
        credited,
        state: record.state,
        enabled: feature_tasks_enabled,
    })
}

/// The onboarding widget
///
/// The user's onboarding profile and progress, the setup checklist, and
/// credit for setup work already done (the email name, signature and
/// Telegram tasks complete with their points, once). `credit` is null unless
/// something was credited just now.
#[utoipa::path(
    get,
    path = "/v1/me/onboarding/setup",
    operation_id = "get_my_onboarding_setup",
    tag = "me",
    responses(
        (status = 200, description = "The widget's data", body = SetupPage),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or of the shop", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn setup(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
) -> ApiResult<Json<SetupPage>> {
    let pool = &state.pool;
    let positions: Vec<(i32, i32)> =
        sqlx::query_as("SELECT position_id, company_id FROM users_positions WHERE user_id = ?")
            .bind(user.id)
            .fetch_all(pool)
            .await?;
    let here = |position: i32| {
        positions
            .iter()
            .any(|(id, company)| *id == position && *company == user.company_id)
    };
    let features = crate::routes::company::queries::company_features(pool, user.company_id).await?;
    let is_admin = user.is_admin || user.is_superuser;
    let setup = setup_status(&state, &user, is_admin).await?;
    let credit = match credit_done(&state, user.id, &setup_done(&setup)).await {
        Ok(CreditOutcome::Applied {
            credited, state, ..
        }) if !credited.is_empty() => Some(Credit {
            task_ids: credited,
            state,
        }),
        // Best effort: the widget renders without the credit.
        Ok(_) | Err(_) => None,
    };
    let record = store::load_record(pool, user.id).await?;
    Ok(Json(SetupPage {
        profile: OnboardingProfile {
            user_id: user.id,
            is_admin,
            can_manage_users: user.is_superuser || here(MANAGER),
            has_cloudtalk_api: features.cloudtalk,
            has_ringcentral_api: features.ringcentral,
            has_inbound_email: features.inbound_email,
            has_cloudtalk_agent: user
                .cloudtalk_agent_id
                .as_deref()
                .is_some_and(|agent| !agent.trim().is_empty()),
            is_shop_worker: here(SHOP_WORKER),
            is_shop_manager: here(SHOP_MANAGER),
            onboarding_completed_at: record.completed_at.clone(),
            saved_progress: record.has_row.then_some(record.state),
        },
        setup,
        credit,
    }))
}

/// Body of `POST /v1/me/onboarding/claim`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ClaimInput {
    /// A setup task (`setup-email-name`, `setup-email-signature`, `setup-telegram`).
    pub task_id: String,
}

/// The claim's outcome.
#[derive(Debug, Serialize, ToSchema)]
pub struct ClaimResult {
    /// `credited`, `already-completed`, `not-done` (open its page instead) or
    /// `unavailable` (guided tours are off).
    pub status: String,
    /// The task credited.
    pub task_id: Option<String>,
    /// The saved state (credited and already-completed).
    pub state: Option<OnboardingState>,
}

/// Claim a setup task
///
/// Completes a setup task, with its points, when the user's own setup shows
/// its work is done; a second claim pays nothing more.
#[utoipa::path(
    post,
    path = "/v1/me/onboarding/claim",
    operation_id = "claim_my_onboarding_task",
    tag = "me",
    request_body = ClaimInput,
    responses(
        (status = 200, description = "The outcome", body = ClaimResult),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or of the shop", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn claim(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    JsonBody(input): JsonBody<ClaimInput>,
) -> ApiResult<Json<ClaimResult>> {
    let task_id = input.task_id.trim();
    let result = |status: &str, task: Option<String>, state: Option<OnboardingState>| ClaimResult {
        status: status.into(),
        task_id: task,
        state,
    };
    if !manifest::is_creditable(task_id) {
        return Ok(Json(result("not-done", None, None)));
    }
    let setup = setup_status(&state, &user, false).await?;
    if !setup_done(&setup).contains(&task_id) {
        return Ok(Json(result("not-done", None, None)));
    }
    Ok(Json(
        match credit_done(&state, user.id, &[task_id]).await? {
            CreditOutcome::Unavailable => result("unavailable", None, None),
            CreditOutcome::NothingPending => {
                let record = store::load_record(&state.pool, user.id).await?;
                result("already-completed", None, Some(record.state))
            }
            CreditOutcome::Applied {
                credited,
                state,
                enabled,
            } => {
                if credited.iter().any(|task| task == task_id) {
                    result("credited", Some(task_id.to_string()), Some(state))
                } else if enabled {
                    result("not-done", None, None)
                } else {
                    result("unavailable", None, None)
                }
            }
        },
    ))
}
