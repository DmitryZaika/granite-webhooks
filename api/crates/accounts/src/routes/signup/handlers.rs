use super::schemas::{
    AddonInfo, BasePlan, Completion, CompletionBody, ConsentsResponse, CreateSignupBody,
    CreatedSignup, PaymentMode, PaymentSetup, PlansQuery, PlansResponse, ResentCode, Signup,
    VerifyBody,
};
use crate::billing::catalog::{
    Catalog, DEFAULT_USERS, FREE_MONTHS, INCLUDED_USERS, MAX_USERS, MIN_USERS, PlanChoice,
    signup_day,
};
use crate::billing::lock::{LockError, NamedLock};
use crate::entitlements::AddonKey;
use crate::error::{ApiError, ApiResult, CodedErrorBody, ErrorBody};
use crate::extract::{JsonBody, MultiQuery};
use crate::routes::auth::{start_session, with_cookies};
use crate::signup::intent::{self, Intent, NewIntent, SignupStatus};
use crate::signup::payment::{self, Setup};
use crate::signup::{
    RequestMeta, consent, email_lock_name, emails, ensure_enabled, funnel, provision,
};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, NaiveDateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::json;
use sha2::Sha256;

/// Seconds between two codes.
const RESEND_SECONDS: i64 = 60;
/// Codes one signup can ask for.
const MAX_CODES: u8 = 5;
/// Wrong codes before the signup locks until a new code.
const MAX_ATTEMPTS: u8 = 5;

fn coded(status: StatusCode, code: &'static str, message: &str) -> ApiError {
    ApiError::coded(status, code, message)
}

fn not_found() -> ApiError {
    coded(StatusCode::NOT_FOUND, "signup_expired", "Signup not found.")
}

fn expired() -> ApiError {
    coded(
        StatusCode::GONE,
        "signup_expired",
        "This signup expired. Start again.",
    )
}

fn invalid_state(message: &str) -> ApiError {
    coded(StatusCode::CONFLICT, "invalid_state", message)
}

/// The signup, or 404 (unknown) / 410 (expired).
async fn live(state: &AppState, id: &str) -> ApiResult<Intent> {
    let intent = intent::load(&state.pool, id).await?.ok_or_else(not_found)?;
    if intent.is_expired(crate::billing::now()) {
        return Err(expired());
    }
    Ok(intent)
}

async fn catalog(state: &AppState) -> ApiResult<Catalog> {
    crate::signup::signup_catalog(&state.stripe).await
}

fn email_masked(email: &str) -> String {
    let (local, domain) = email.split_once('@').unwrap_or((email, ""));
    format!("{}***@{domain}", local.chars().next().unwrap_or('*'))
}

fn resend_in(intent: &Intent, now: NaiveDateTime) -> i64 {
    intent.code_sent_at.map_or(0, |sent| {
        let left = (sent + chrono::Duration::seconds(RESEND_SECONDS) - now).num_milliseconds();
        if left <= 0 {
            0
        } else {
            ((left + 999) / 1000).min(RESEND_SECONDS)
        }
    })
}

fn view(state: &AppState, intent: &Intent, catalog: &Catalog, now: DateTime<Utc>) -> Signup {
    Signup {
        id: intent.id.clone(),
        status: intent.status,
        email_masked: email_masked(&intent.email),
        company_name: intent.company.name.clone(),
        plan: intent.plan.clone(),
        quote: catalog.quote(&intent.plan, signup_day(now)),
        payment_mode: if payment::is_test_mode(state) {
            PaymentMode::Test
        } else {
            PaymentMode::Stripe
        },
        resend_in_seconds: resend_in(intent, now.naive_utc()),
    }
}

async fn signup_view(state: &AppState, intent: &Intent) -> ApiResult<Json<Signup>> {
    let catalog = catalog(state).await?;
    Ok(Json(view(state, intent, &catalog, Utc::now())))
}

/// A uniform 6-digit code (rejection sampling, no modulo bias).
fn new_code() -> ApiResult<String> {
    const LIMIT: u32 = 4_294_000_000;
    loop {
        let mut bytes = [0_u8; 4];
        getrandom::fill(&mut bytes).map_err(|_| ApiError::Internal("no randomness".into()))?;
        let value = u32::from_be_bytes(bytes);
        if value < LIMIT {
            return Ok(format!("{:06}", value % 1_000_000));
        }
    }
}

fn code_mac(state: &AppState, id: &str, code: &str) -> Hmac<Sha256> {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(state.session_secret.as_bytes())
        .expect("HMAC takes any key");
    mac.update(format!("{id}:{code}").as_bytes());
    mac
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// `HMAC-SHA256(session secret, "<id>:<code>")` as hex.
fn code_hash(state: &AppState, id: &str, code: &str) -> String {
    hex(&code_mac(state, id, code).finalize().into_bytes())
}

/// Whether `code` is the one `stored` is the hash of (constant time).
fn code_matches(state: &AppState, id: &str, code: &str, stored: &str) -> bool {
    let Some(stored) = (0..stored.len())
        .step_by(2)
        .map(|at| {
            stored
                .get(at..at + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        })
        .collect::<Option<Vec<u8>>>()
    else {
        return false;
    };
    code_mac(state, id, code).verify_slice(&stored).is_ok()
}

fn addon_keys(plan: &PlanChoice) -> Vec<&'static str> {
    plan.addons.iter().map(|key| key.as_str()).collect()
}

const fn addon_description(key: AddonKey) -> &'static str {
    match key {
        AddonKey::Signatures => "eSignatures and an email signature block",
        AddonKey::Quotes => "Quote editor, price lists and online quotes",
        AddonKey::Quickbooks => "Invoices and QuickBooks sync",
    }
}

/// List plans and price a choice
///
/// The self-serve catalog (base plan, price of each extra user, add-ons, free
/// months, user bounds) and a quote for the choice in the query. Money is in
/// cents, before sales tax. `users` defaults to 5 and is clamped to 1..=500;
/// repeat `addon` for each add-on and unknown values are ignored. Works even
/// while signups are switched off. No session needed.
#[crate::openapi::path(
    get,
    path = "/v1/public/plans",
    operation_id = "get_public_plans",
    tag = "signup",
    params(PlansQuery),
    responses(
        (status = 200, description = "The catalog and the quote", body = PlansResponse),
        (status = 400, description = "Malformed query string", body = ErrorBody),
        (status = 503, description = "Prices are unavailable (code signup_unavailable)", body = CodedErrorBody),
    )
)]
pub async fn plans(
    State(state): State<AppState>,
    MultiQuery(query): MultiQuery<PlansQuery>,
) -> ApiResult<Json<PlansResponse>> {
    let users = query
        .users
        .as_deref()
        .and_then(|users| users.trim().parse::<i64>().ok())
        .unwrap_or(DEFAULT_USERS);
    let addons: Vec<AddonKey> = query
        .addon
        .iter()
        .filter_map(|value| AddonKey::parse(value))
        .collect();
    let plan = PlanChoice::normalized(users, &addons);
    let catalog = catalog(&state).await?;
    let quote = catalog.quote(&plan, signup_day(Utc::now()));
    Ok(Json(PlansResponse {
        base: BasePlan {
            name: "Base plan".into(),
            monthly_cents: catalog.base.unit_cents,
            included_users: INCLUDED_USERS,
        },
        extra_user_cents: catalog.extra_user.unit_cents,
        free_months: FREE_MONTHS,
        min_users: MIN_USERS,
        max_users: MAX_USERS,
        addons: AddonKey::ALL
            .into_iter()
            .map(|key| AddonInfo {
                key,
                name: key.label().into(),
                description: addon_description(key).into(),
                monthly_cents: catalog.addon(key).unit_cents,
            })
            .collect(),
        quote,
    }))
}

fn lock_error(err: LockError) -> ApiError {
    match err {
        LockError::Busy => coded(
            StatusCode::SERVICE_UNAVAILABLE,
            "signup_unavailable",
            "Signup is busy. Try again in a moment.",
        ),
        LockError::Database(err) => ApiError::Database(err),
    }
}

/// Which message an existing account's email gets.
async fn email_conflict(state: &AppState, email: &str) -> ApiResult<Option<ApiError>> {
    let deleted: Option<bool> = sqlx::query_scalar(
        "SELECT is_deleted FROM users WHERE LOWER(TRIM(email)) = ?
          ORDER BY is_deleted ASC LIMIT 1",
    )
    .bind(email)
    .fetch_optional(&state.pool)
    .await?;
    Ok(deleted.map(|deleted| {
        let message = if deleted {
            "This email belongs to a deleted account. Use a different email."
        } else {
            "An account already uses this email."
        };
        coded(StatusCode::CONFLICT, "email_taken", message)
    }))
}

/// Start a signup
///
/// Step 1 of self-serve signup. Checks the company and owner details, refuses
/// an email that already has an account (deleted accounts included), stores
/// the signup and emails a 6-digit code (valid 15 minutes). The returned `id`
/// is the secret of the rest of the flow: next call `POST
/// /v1/public/signups/{id}/verification` with the code. The signup expires
/// after 24 hours. Answers 503 while signups are switched off.
#[crate::openapi::path(
    post,
    path = "/v1/public/signups",
    operation_id = "create_signup",
    tag = "signup",
    request_body = CreateSignupBody,
    responses(
        (status = 201, description = "Signup created, code emailed", body = CreatedSignup),
        (status = 400, description = "A field is missing or invalid; the message names it", body = ErrorBody),
        (status = 409, description = "An account already uses the email (code email_taken)", body = CodedErrorBody),
        (status = 502, description = "The code email could not be sent; the signup was dropped", body = ErrorBody),
        (status = 503, description = "Signups are switched off, busy, or prices unavailable (code signup_unavailable)", body = CodedErrorBody),
    )
)]
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    JsonBody(body): JsonBody<CreateSignupBody>,
) -> ApiResult<(StatusCode, Json<CreatedSignup>)> {
    ensure_enabled(&state.signup)?;
    let valid = body.validate()?;
    // Fails closed (503) before any write when the prices are unavailable or
    // no longer match the consent texts.
    catalog(&state).await?;
    let password = valid.password.clone();
    let password_hash = tokio::task::spawn_blocking(move || bcrypt::hash(password, 10))
        .await
        .map_err(|_| ApiError::Internal("password hashing failed".into()))?
        .map_err(|_| ApiError::BadRequest("Password has a character we cannot accept.".into()))?;
    let id = uuid::Uuid::new_v4().to_string();
    let code = new_code()?;
    let meta = RequestMeta::from_headers(&headers);

    let lock = NamedLock::acquire(&state.pool, &email_lock_name(&valid.email), 10)
        .await
        .map_err(lock_error)?;
    let stored = async {
        if let Some(conflict) = email_conflict(&state, &valid.email).await? {
            return Err(conflict);
        }
        let new = NewIntent {
            id: &id,
            email: &valid.email,
            code_hash: &code_hash(&state, &id, &code),
            company: &valid.company,
            owner_name: &valid.owner_name,
            password_hash: &password_hash,
            plan: &valid.plan,
            ip: meta.ip.as_deref(),
            user_agent: meta.user_agent.as_deref(),
            now: crate::billing::now(),
        };
        intent::insert(&state.pool, &new).await?;
        Ok(())
    }
    .await;
    lock.release().await;
    stored?;

    if let Err(err) = emails::send_code(&state, &valid.email, &code).await {
        intent::expire(&state.pool, &id, crate::billing::now()).await?;
        return Err(err);
    }
    funnel::capture(
        &state,
        "signup_started",
        &id,
        json!({ "users_estimate": valid.plan.users, "addons": addon_keys(&valid.plan) }),
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(CreatedSignup {
            id,
            resend_in_seconds: RESEND_SECONDS,
        }),
    ))
}

/// Read a signup
///
/// The state of a signup: status, masked email, company name, plan, today's
/// quote, how payment will be collected (`payment_mode`) and seconds until
/// another code can be sent. Never returns the full email. 404 for an unknown
/// id, 410 once expired (both code `signup_expired`: start again).
#[crate::openapi::path(
    get,
    path = "/v1/public/signups/{id}",
    operation_id = "get_signup",
    tag = "signup",
    params(("id" = String, Path, description = "Signup id from `create_signup`")),
    responses(
        (status = 200, description = "The signup", body = Signup),
        (status = 404, description = "Unknown signup (code signup_expired)", body = CodedErrorBody),
        (status = 410, description = "Expired signup (code signup_expired)", body = CodedErrorBody),
        (status = 503, description = "Signups are switched off or prices unavailable (code signup_unavailable)", body = CodedErrorBody),
    )
)]
pub async fn get_signup(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Signup>> {
    ensure_enabled(&state.signup)?;
    let intent = live(&state, &id).await?;
    signup_view(&state, &intent).await
}

/// Change the plan of a signup
///
/// Replaces the user estimate and add-ons ("Change plan" in step 3). Allowed
/// until payment is set up and the signup is completed: statuses `started`,
/// `verified` and `payment_ready`; otherwise 409 `invalid_state`. Users are
/// clamped to 1..=500. The consent texts change with the price: fetch them
/// again with `get_signup_consents`.
#[crate::openapi::path(
    put,
    path = "/v1/public/signups/{id}/plan",
    operation_id = "update_signup_plan",
    tag = "signup",
    params(("id" = String, Path, description = "Signup id from `create_signup`")),
    request_body = PlanChoice,
    responses(
        (status = 200, description = "The signup with the new plan and quote", body = Signup),
        (status = 400, description = "Malformed body", body = ErrorBody),
        (status = 404, description = "Unknown signup (code signup_expired)", body = CodedErrorBody),
        (status = 409, description = "The signup can no longer change plan (code invalid_state)", body = CodedErrorBody),
        (status = 410, description = "Expired signup (code signup_expired)", body = CodedErrorBody),
        (status = 503, description = "Signups are switched off or prices unavailable (code signup_unavailable)", body = CodedErrorBody),
    )
)]
pub async fn update_plan(
    State(state): State<AppState>,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<PlanChoice>,
) -> ApiResult<Json<Signup>> {
    ensure_enabled(&state.signup)?;
    let mut intent = live(&state, &id).await?;
    let plan = PlanChoice::normalized(body.users, &body.addons);
    if !intent::set_plan(&state.pool, &intent.id, &plan, crate::billing::now()).await? {
        return Err(invalid_state("This signup can no longer change its plan."));
    }
    intent.plan = plan;
    funnel::capture(
        &state,
        "signup_plan_changed",
        &id,
        json!({ "users_estimate": intent.plan.users, "addons": addon_keys(&intent.plan) }),
    )
    .await;
    signup_view(&state, &intent).await
}

/// Send the email code again
///
/// Emails a new 6-digit code (the old one stops working and the wrong-code
/// count restarts). Only while the email is not verified yet. At most one per
/// 60 seconds (429 `resend_limited` with `retry_after_seconds`) and 5 codes per
/// signup (429 `resend_limited`).
#[crate::openapi::path(
    post,
    path = "/v1/public/signups/{id}/code",
    operation_id = "resend_signup_code",
    tag = "signup",
    params(("id" = String, Path, description = "Signup id from `create_signup`")),
    responses(
        (status = 200, description = "A new code was emailed", body = ResentCode),
        (status = 404, description = "Unknown signup (code signup_expired)", body = CodedErrorBody),
        (status = 409, description = "The email is already verified (code invalid_state)", body = CodedErrorBody),
        (status = 410, description = "Expired signup (code signup_expired)", body = CodedErrorBody),
        (status = 429, description = "Too soon or too many codes (code resend_limited; retry_after_seconds when too soon)", body = CodedErrorBody),
        (status = 502, description = "The email could not be sent", body = ErrorBody),
        (status = 503, description = "Signups are switched off (code signup_unavailable)", body = CodedErrorBody),
    )
)]
pub async fn resend_code(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<ResentCode>> {
    ensure_enabled(&state.signup)?;
    let intent = live(&state, &id).await?;
    if intent.status != SignupStatus::Started {
        return Err(invalid_state("Your email is already verified."));
    }
    let code = new_code()?;
    let now = crate::billing::now();
    let updated = sqlx::query(
        "UPDATE signup_intents
            SET codes_sent = codes_sent + 1, code_attempts = 0, code_hash = ?,
                code_sent_at = ?, code_expires_at = ?, updated_at = ?
          WHERE id = ? AND status = 'started' AND codes_sent < ? AND code_sent_at <= ?",
    )
    .bind(code_hash(&state, &intent.id, &code))
    .bind(now)
    .bind(now + chrono::Duration::minutes(intent::CODE_MINUTES))
    .bind(now)
    .bind(&intent.id)
    .bind(MAX_CODES)
    .bind(now - chrono::Duration::seconds(RESEND_SECONDS))
    .execute(&state.pool)
    .await?;
    if updated.rows_affected() != 1 {
        let current = live(&state, &id).await?;
        if current.status != SignupStatus::Started {
            return Err(invalid_state("Your email is already verified."));
        }
        let wait = resend_in(&current, crate::billing::now()).max(1);
        if current.codes_sent < MAX_CODES {
            return Err(coded(
                StatusCode::TOO_MANY_REQUESTS,
                "resend_limited",
                "Wait a minute before asking for another code.",
            )
            .with_field("retry_after_seconds", wait));
        }
        return Err(coded(
            StatusCode::TOO_MANY_REQUESTS,
            "resend_limited",
            "You have asked for 5 codes. Start again or email support@granite-manager.com.",
        ));
    }
    emails::send_code(&state, &intent.email, &code).await?;
    funnel::capture(&state, "signup_code_resent", &id, json!({})).await;
    Ok(Json(ResentCode {
        resend_in_seconds: RESEND_SECONDS,
    }))
}

fn too_many_attempts() -> ApiError {
    coded(
        StatusCode::TOO_MANY_REQUESTS,
        "too_many_attempts",
        "Too many wrong codes. Send a new code.",
    )
}

/// Verify the email code
///
/// Checks the 6-digit code from the email and moves the signup from `started`
/// to `verified`. Five wrong codes lock the signup (429 `too_many_attempts`,
/// even for the right code) until `resend_signup_code` sends a new one. A wrong
/// code answers 422 `invalid_code` with `attempts_left`; an old one 422
/// `code_expired`. An already verified signup is returned as is.
#[crate::openapi::path(
    post,
    path = "/v1/public/signups/{id}/verification",
    operation_id = "verify_signup_code",
    tag = "signup",
    params(("id" = String, Path, description = "Signup id from `create_signup`")),
    request_body = VerifyBody,
    responses(
        (status = 200, description = "The verified signup", body = Signup),
        (status = 400, description = "Malformed body", body = ErrorBody),
        (status = 404, description = "Unknown signup (code signup_expired)", body = CodedErrorBody),
        (status = 409, description = "The signup is past verification (code invalid_state)", body = CodedErrorBody),
        (status = 410, description = "Expired signup (code signup_expired)", body = CodedErrorBody),
        (status = 422, description = "Wrong or old code (codes invalid_code with attempts_left, code_expired)", body = CodedErrorBody),
        (status = 429, description = "Five wrong codes (code too_many_attempts)", body = CodedErrorBody),
        (status = 503, description = "Signups are switched off or prices unavailable (code signup_unavailable)", body = CodedErrorBody),
    )
)]
pub async fn verify_code(
    State(state): State<AppState>,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<VerifyBody>,
) -> ApiResult<Json<Signup>> {
    ensure_enabled(&state.signup)?;
    let intent = live(&state, &id).await?;
    match intent.status {
        SignupStatus::Started => {}
        SignupStatus::Verified
        | SignupStatus::PaymentReady
        | SignupStatus::AwaitingBankVerification => return signup_view(&state, &intent).await,
        _ => return Err(invalid_state("This signup is already past verification.")),
    }
    let now = crate::billing::now();
    if intent.code_attempts >= MAX_ATTEMPTS {
        return Err(too_many_attempts());
    }
    let stored = match (&intent.code_hash, intent.code_expires_at) {
        (Some(hash), Some(expires)) if expires > now => hash,
        _ => {
            return Err(coded(
                StatusCode::UNPROCESSABLE_ENTITY,
                "code_expired",
                "That code expired. Send a new one.",
            ));
        }
    };
    let counted = sqlx::query(
        "UPDATE signup_intents
            SET code_attempts = LAST_INSERT_ID(code_attempts + 1), updated_at = ?
          WHERE id = ? AND status = 'started' AND code_attempts < ?",
    )
    .bind(now)
    .bind(&intent.id)
    .bind(MAX_ATTEMPTS)
    .execute(&state.pool)
    .await?;
    if counted.rows_affected() != 1 {
        let current = live(&state, &id).await?;
        return if current.status == SignupStatus::Started {
            Err(too_many_attempts())
        } else {
            signup_view(&state, &current).await
        };
    }
    let used = u8::try_from(counted.last_insert_id()).unwrap_or(MAX_ATTEMPTS);
    let submitted = body.code.trim();
    let right = submitted.len() == 6
        && submitted.bytes().all(|b| b.is_ascii_digit())
        && code_matches(&state, &intent.id, submitted, stored);
    if !right {
        if used >= MAX_ATTEMPTS {
            return Err(too_many_attempts());
        }
        return Err(coded(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_code",
            "That code doesn't match.",
        )
        .with_field("attempts_left", MAX_ATTEMPTS - used));
    }
    let moved = intent::mark_verified(&state.pool, &intent.id, crate::billing::now()).await?;
    let current = live(&state, &id).await?;
    if !moved && current.status == SignupStatus::Started {
        return Err(invalid_state("Verify your email first."));
    }
    if moved {
        funnel::capture(&state, "signup_verified", &id, json!({})).await;
    }
    signup_view(&state, &current).await
}

/// Read the consent texts
///
/// The four checkboxes (terms, auto_renewal, per_user_pricing, onboarding_fee)
/// rendered by the server for this signup's plan and dates, in the order to
/// show them. The page shows `text` verbatim and sends `kind`, `version` and
/// `sha256` back as `text_sha256` to `complete_signup`. Texts change when the
/// plan or the prices do: fetch them again after `update_signup_plan`. Only
/// after the email is verified (409 `invalid_state` before).
#[crate::openapi::path(
    get,
    path = "/v1/public/signups/{id}/consents",
    operation_id = "get_signup_consents",
    tag = "signup",
    params(("id" = String, Path, description = "Signup id from `create_signup`")),
    responses(
        (status = 200, description = "The checkbox texts", body = ConsentsResponse),
        (status = 404, description = "Unknown signup (code signup_expired)", body = CodedErrorBody),
        (status = 409, description = "The email is not verified yet (code invalid_state)", body = CodedErrorBody),
        (status = 410, description = "Expired signup (code signup_expired)", body = CodedErrorBody),
        (status = 503, description = "Signups are switched off or prices unavailable (code signup_unavailable)", body = CodedErrorBody),
    )
)]
pub async fn consents(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<ConsentsResponse>> {
    ensure_enabled(&state.signup)?;
    let intent = live(&state, &id).await?;
    if !matches!(
        intent.status,
        SignupStatus::Verified | SignupStatus::PaymentReady
    ) {
        return Err(invalid_state("Verify your email first."));
    }
    let catalog = catalog(&state).await?;
    let facts = consent::facts(&catalog, &intent, Utc::now());
    Ok(Json(ConsentsResponse {
        consents: consent::render(&facts, &state.app_url),
    }))
}

/// Set up payment
///
/// Creates the signup's Stripe customer (once) and a new SetupIntent, and moves
/// the signup to `payment_ready`. Answers `{"mode": "test"}` when the api
/// attached a sandbox payment method itself, or `{"mode": "stripe",
/// "client_secret", "publishable_key"}` for the page to confirm with Stripe's
/// Payment Element. Call again after a failed confirmation: at most 5 setups
/// per signup (429 `setup_limited`). Only for verified signups.
#[crate::openapi::path(
    post,
    path = "/v1/public/signups/{id}/payment-setup",
    operation_id = "create_signup_payment_setup",
    tag = "signup",
    params(("id" = String, Path, description = "Signup id from `create_signup`")),
    responses(
        (status = 200, description = "How to collect the payment method", body = PaymentSetup),
        (status = 404, description = "Unknown signup (code signup_expired)", body = CodedErrorBody),
        (status = 409, description = "The email is not verified yet (code invalid_state)", body = CodedErrorBody),
        (status = 410, description = "Expired signup (code signup_expired)", body = CodedErrorBody),
        (status = 429, description = "Five setups used (code setup_limited)", body = CodedErrorBody),
        (status = 502, description = "Stripe failed; try again", body = ErrorBody),
        (status = 503, description = "Signups or card payments are not available (code signup_unavailable)", body = CodedErrorBody),
    )
)]
pub async fn payment_setup(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<PaymentSetup>> {
    ensure_enabled(&state.signup)?;
    let intent = live(&state, &id).await?;
    if !matches!(
        intent.status,
        SignupStatus::Verified | SignupStatus::PaymentReady
    ) {
        return Err(invalid_state("Verify your email first."));
    }
    let setup = payment::setup(&state, &intent).await?;
    let (mode, body) = match setup {
        Setup::Test => (
            "test",
            PaymentSetup {
                mode: PaymentMode::Test,
                client_secret: None,
                publishable_key: None,
            },
        ),
        Setup::Stripe {
            client_secret,
            publishable_key,
        } => (
            "stripe",
            PaymentSetup {
                mode: PaymentMode::Stripe,
                client_secret: Some(client_secret),
                publishable_key: Some(publishable_key),
            },
        ),
    };
    funnel::capture(&state, "signup_payment_setup", &id, json!({ "mode": mode })).await;
    Ok(Json(body))
}

/// Complete the signup
///
/// Final step. Sends the checkboxes the buyer ticked (`kind`, `version`,
/// `text_sha256` as `get_signup_consents` returned them); the server renders
/// them again and refuses with 409 `consents_changed` when anything differs
/// (fetch the texts again). Then creates the company, the owner account and the
/// subscription with a free period, signs the owner in (the session cookie is
/// set on this response) and returns where to go next. Safe to retry: one
/// signup never makes two companies. 409 codes: `consents_changed`,
/// `invalid_state`, `email_taken`, `payment_not_ready`; 503 `provisioning_retry`
/// means call again shortly.
#[crate::openapi::path(
    post,
    path = "/v1/public/signups/{id}/completion",
    operation_id = "complete_signup",
    tag = "signup",
    params(("id" = String, Path, description = "Signup id from `create_signup`")),
    request_body = CompletionBody,
    responses(
        (status = 200, description = "The company exists; Set-Cookie signs the owner in unless the signup was already completed", body = Completion),
        (status = 400, description = "Malformed body", body = ErrorBody),
        (status = 404, description = "Unknown signup (code signup_expired)", body = CodedErrorBody),
        (status = 409, description = "Texts changed, wrong state, email taken or payment not ready (codes consents_changed, invalid_state, email_taken, payment_not_ready)", body = CodedErrorBody),
        (status = 410, description = "Expired signup (code signup_expired)", body = CodedErrorBody),
        (status = 502, description = "Stripe failed; try again", body = ErrorBody),
        (status = 503, description = "Switched off or still provisioning (codes signup_unavailable, provisioning_retry)", body = CodedErrorBody),
    )
)]
pub async fn complete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    JsonBody(body): JsonBody<CompletionBody>,
) -> ApiResult<Response> {
    ensure_enabled(&state.signup)?;
    let outcome = provision::complete(
        &state,
        &id,
        &body.consents,
        &RequestMeta::from_headers(&headers),
    )
    .await?;
    let response = Json(Completion {
        redirect_url: outcome.redirect_url,
    })
    .into_response();
    match outcome.sign_in {
        Some(user_id) => {
            let session = start_session(&state, user_id).await?;
            Ok(with_cookies(response, session.cookies))
        }
        None => Ok(response),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_the_email() {
        assert_eq!(email_masked("anna@acme.com"), "a***@acme.com");
    }

    #[test]
    fn codes_are_six_digits() {
        for _ in 0..50 {
            let code = new_code().ok().unwrap();
            assert_eq!(code.len(), 6);
            assert!(code.bytes().all(|b| b.is_ascii_digit()));
        }
    }
}
