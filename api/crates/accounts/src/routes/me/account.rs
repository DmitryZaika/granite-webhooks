//! `GET`/`PUT /v1/me/account`: the user's own account (My Account).
//!
//! Name, contact details, password, the email sender name and signature, and
//! the texting line. Replaces the Remix `employee.user` and `shop.user`
//! loaders and actions.

use crate::auth::ShopOrEmployeeUser;
use crate::auth::cookie::request_session_id;
use crate::email_signature;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::phone::company as phone_company;
use crate::phone::lines;
use crate::phone::settings as phone_settings;
use crate::routes::company::queries::company_features;
use crate::routes::onboarding::{self, Credit, CreditOutcome, store};
use crate::routes::users::manage::resolve_ringcentral_extension;
use crate::routes::users::texting_history;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;

const SHOP_MANAGER: i32 = 3;
const SHOP_WORKER: i32 = 4;
const BCRYPT_COST: u32 = 10;
const MIN_PASSWORD: usize = 8;

/// `GET /v1/me/account`.
#[derive(Debug, Serialize, ToSchema)]
pub struct Account {
    /// The user.
    pub user_id: i32,
    /// Name.
    pub name: Option<String>,
    /// Sign-in email.
    pub email: Option<String>,
    /// Phone.
    pub phone_number: Option<String>,
    /// The name recipients see in the From field.
    pub email_name: Option<String>,
    /// The email signature (sanitized HTML).
    pub email_signature: Option<String>,
    /// `CloudTalk` agent id.
    pub cloudtalk_agent_id: Option<String>,
    /// `CloudTalk` line (10 digits).
    pub cloudtalk_phone_number: Option<String>,
    /// `RingCentral` extension of the line.
    pub ringcentral_extension_id: Option<String>,
    /// `RingCentral` line (10 digits).
    pub ringcentral_phone_number: Option<String>,
    /// The home company has `CloudTalk`.
    pub has_cloudtalk_api: bool,
    /// The home company has `RingCentral`.
    pub has_ringcentral_api: bool,
    /// The signature applies (not for shop managers and workers).
    pub shows_email_signature: bool,
    /// The user already rated the guided tours.
    pub has_onboarding_feedback: bool,
    /// The user dismissed the tours rating.
    pub feedback_dismissed: bool,
    /// The home company calls through Granite Phone (Telnyx calling is on).
    pub has_granite_phone: bool,
    /// The Granite Phone numbers the user can call from (E.164); an admin
    /// assigns them on the Phone page.
    pub granite_phone_numbers: Vec<String>,
    /// The user's calls say "This call is being recorded." to both sides when
    /// their recording starts (`PUT /v1/me/recording-voice`).
    pub recording_voice: bool,
}

/// Body of `PUT /v1/me/account`. A field left out keeps its value; an empty
/// one clears it.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountInput {
    /// Name (required).
    pub name: String,
    /// Sign-in email (required).
    pub email: String,
    /// Phone (at least 10 digits).
    #[serde(default)]
    pub phone_number: Option<String>,
    /// A new password (at least 8 characters); empty keeps the current one.
    #[serde(default)]
    pub password: Option<String>,
    /// The email sender name.
    #[serde(default)]
    pub email_name: Option<String>,
    /// The email signature (HTML, sanitized on save).
    #[serde(default)]
    pub email_signature: Option<String>,
    /// `CloudTalk` agent id (digits).
    #[serde(default)]
    pub cloudtalk_agent_id: Option<String>,
    /// `CloudTalk` line (10+ digits).
    #[serde(default)]
    pub cloudtalk_phone_number: Option<String>,
    /// `RingCentral` line (10+ digits); the extension is looked up from it.
    #[serde(default)]
    pub ringcentral_phone_number: Option<String>,
}

/// What a save did.
#[derive(Debug, Serialize, ToSchema)]
pub struct AccountSaved {
    /// The texting line: `saved`, `unchanged`, `unmatched` (the provider did
    /// not confirm it), `incomplete` (a `CloudTalk` line without its agent id)
    /// or `none` (no line submitted).
    pub texting_line_status: String,
    /// The provider of that line (`CloudTalk`, `RingCentral`); null for `none`.
    pub texting_line_provider: Option<String>,
    /// Setup tasks this save finished, with their points; null when none.
    pub credit: Option<Credit>,
}

#[derive(Debug, Default, sqlx::FromRow)]
struct Stored {
    name: Option<String>,
    email: Option<String>,
    phone_number: Option<String>,
    email_name: Option<String>,
    email_signature: Option<String>,
    cloudtalk_agent_id: Option<String>,
    cloudtalk_phone_number: Option<String>,
    ringcentral_extension_id: Option<String>,
    ringcentral_phone_number: Option<String>,
}

impl Stored {
    /// The stored texting line.
    fn line(&self) -> texting_history::Line {
        texting_history::Line {
            cloudtalk_agent: self.cloudtalk_agent_id.clone(),
            cloudtalk_phone: line10(self.cloudtalk_phone_number.as_deref()),
            ringcentral_extension: self.ringcentral_extension_id.clone(),
            ringcentral_phone: line10(self.ringcentral_phone_number.as_deref()),
        }
    }
}

async fn stored(pool: &MySqlPool, actor: store::Actor) -> ApiResult<Stored> {
    sqlx::query_as(
        "SELECT name, email, phone_number, email_name, email_signature,
                cloudtalk_agent_id, cloudtalk_phone_number,
                ringcentral_extension_id, ringcentral_phone_number
           FROM users WHERE id = ? AND company_id = ? AND is_deleted = 0",
    )
    .bind(actor.user_id)
    .bind(actor.company_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| ApiError::Unauthenticated("Sign in again.".into()))
}

async fn home_actor(pool: &MySqlPool, user_id: i32) -> ApiResult<store::Actor> {
    store::actor(pool, user_id)
        .await?
        .ok_or_else(|| ApiError::Unauthenticated("Sign in again.".into()))
}

/// My account
///
/// The user's details in their home company, and what the page shows: the
/// texting providers, the signature, and the guided tours rating.
#[crate::openapi::path(
    get,
    path = "/v1/me/account",
    operation_id = "get_my_account",
    tag = "me",
    responses(
        (status = 200, description = "The account", body = Account),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or of the shop", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn account(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
) -> ApiResult<Json<Account>> {
    let pool = &state.pool;
    let actor = home_actor(pool, user.id).await?;
    let row = stored(pool, actor).await?;
    let features = company_features(pool, actor.company_id).await?;
    let shop: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users_positions
                        WHERE user_id = ? AND company_id = ? AND position_id IN (?, ?))",
    )
    .bind(actor.user_id)
    .bind(actor.company_id)
    .bind(SHOP_MANAGER)
    .bind(SHOP_WORKER)
    .fetch_one(pool)
    .await?;
    let has_onboarding_feedback: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM onboarding_feedback WHERE user_id = ?)")
            .bind(actor.user_id)
            .fetch_one(pool)
            .await?;
    let progress = store::load_record(pool, actor.user_id).await?;
    let has_granite_phone = phone_company::load(pool, actor.company_id)
        .await?
        .is_some_and(|config| config.calling_on());
    let granite_phone_numbers = if has_granite_phone {
        lines::caller_ids(pool, actor.company_id, actor.user_id)
            .await?
            .into_iter()
            .map(|line| line.e164)
            .collect()
    } else {
        Vec::new()
    };
    let recording_voice =
        phone_settings::recording_voice(pool, actor.user_id, actor.company_id).await?;
    Ok(Json(Account {
        user_id: actor.user_id,
        name: row.name,
        email: row.email,
        phone_number: row.phone_number,
        email_name: row.email_name,
        email_signature: row.email_signature,
        cloudtalk_agent_id: row.cloudtalk_agent_id,
        cloudtalk_phone_number: row.cloudtalk_phone_number,
        ringcentral_extension_id: row.ringcentral_extension_id,
        ringcentral_phone_number: row.ringcentral_phone_number,
        has_cloudtalk_api: features.cloudtalk,
        has_ringcentral_api: features.ringcentral,
        shows_email_signature: !shop,
        has_onboarding_feedback,
        feedback_dismissed: progress.has_row && progress.state.feedback_dismissed,
        has_granite_phone,
        granite_phone_numbers,
        recording_voice,
    }))
}

/// Body of `PUT /v1/me/recording-voice`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct RecordingVoiceSwitch {
    /// Say the recording notice on my calls.
    pub on: bool,
}

/// Turn the recording voice of my calls on or off
///
/// My Account > Phone lines > Granite Manager. Off: the user's outgoing
/// Granite Phone calls, Call via my cell calls and the Record button record
/// without saying "This call is being recorded." (some states require
/// telling the other side). Incoming calls keep their notice. 403 when the
/// home company does not call through Granite Phone.
#[crate::openapi::path(
    put,
    path = "/v1/me/recording-voice",
    operation_id = "set_my_recording_voice",
    tag = "me",
    request_body = RecordingVoiceSwitch,
    responses(
        (status = 204, description = "Saved"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or of the shop, or the company has no Granite Phone", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_recording_voice(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    JsonBody(input): JsonBody<RecordingVoiceSwitch>,
) -> ApiResult<axum::http::StatusCode> {
    let pool = &state.pool;
    let actor = home_actor(pool, user.id).await?;
    let calling = phone_company::load(pool, actor.company_id)
        .await?
        .is_some_and(|config| config.calling_on());
    if !calling {
        return Err(ApiError::Denied(
            "Your company doesn't call through Granite Phone".into(),
        ));
    }
    phone_settings::set_recording_voice(pool, actor.user_id, actor.company_id, input.on).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// A submitted field: left out (the stored value stays), or set (empty
/// clears it).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Field {
    Keep,
    Set(Option<String>),
}

impl Field {
    /// Trimmed; an empty value clears.
    fn from_input(value: Option<&String>) -> Self {
        value.map_or(Self::Keep, |v| {
            Self::Set(Some(v.trim().to_string()).filter(|v| !v.is_empty()))
        })
    }

    /// The submitted value, when one was.
    fn value(&self) -> Option<&str> {
        match self {
            Self::Set(Some(value)) => Some(value),
            _ => None,
        }
    }

    const fn submitted(&self) -> bool {
        matches!(self, Self::Set(_))
    }

    fn map(self, f: impl FnOnce(&str) -> Option<String>) -> Self {
        match self {
            Self::Set(Some(value)) => Self::Set(f(&value)),
            other => other,
        }
    }

    /// The value to store.
    fn or_kept(&self, stored: Option<&String>) -> Option<String> {
        match self {
            Self::Keep => stored.cloned(),
            Self::Set(value) => value.clone(),
        }
    }
}

/// The last 10 digits of a number with at least 10.
fn line10(value: Option<&str>) -> Option<String> {
    let digits: String = value?.chars().filter(char::is_ascii_digit).collect();
    (digits.len() >= 10).then(|| digits[digits.len() - 10..].to_string())
}

/// A digits-only field of at least `min` digits.
fn digits_field(value: Option<&str>, label: &str, min: usize) -> ApiResult<()> {
    let Some(value) = value else { return Ok(()) };
    if !value.chars().all(|c| c.is_ascii_digit()) {
        return Err(ApiError::BadRequest(format!(
            "{label} must be numbers only"
        )));
    }
    if value.len() < min {
        return Err(ApiError::BadRequest(format!(
            "{label} must include at least {min} digits"
        )));
    }
    Ok(())
}

/// One address: no whitespace, no characters that are SQL patterns or
/// address-list syntax (`%`, `<`, `,`, ...), a domain of letters, digits,
/// `-` and dots. Mail and notifications are matched to users by address.
pub fn valid_email(email: &str) -> bool {
    email.len() <= 254
        && !email
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "%<>,;:\"()[]\\".contains(c))
        && email.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty()
                && !domain.contains('@')
                && domain.split('.').count() >= 2
                && domain.split('.').all(|part| !part.is_empty())
                && domain
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '-' || c == '.')
        })
}

/// The fields of a save, checked and normalized.
struct Checked {
    name: String,
    email: String,
    phone_number: Field,
    password: Option<String>,
    email_name: Field,
    email_signature: Field,
    cloudtalk_agent_id: Field,
    /// 10 digits.
    cloudtalk_phone: Field,
    /// 10 digits.
    ringcentral_phone: Field,
}

fn check(input: &AccountInput) -> ApiResult<Checked> {
    let name = input.name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError::BadRequest("Name is required".into()));
    }
    let phone_number = Field::from_input(input.phone_number.as_ref());
    if phone_number
        .value()
        .is_some_and(|phone| phone.chars().filter(char::is_ascii_digit).count() < 10)
    {
        return Err(ApiError::BadRequest(
            "Phone must include at least 10 digits".into(),
        ));
    }
    let email = input.email.trim().to_lowercase();
    if !valid_email(&email) {
        return Err(ApiError::BadRequest("Invalid email address".into()));
    }
    let password = input
        .password
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string);
    if password
        .as_ref()
        .is_some_and(|p| p.chars().count() < MIN_PASSWORD || p.len() > 200)
    {
        return Err(ApiError::BadRequest(
            "Password must be at least 8 characters long".into(),
        ));
    }
    if input
        .email_signature
        .as_ref()
        .is_some_and(|s| s.len() > 100_000)
    {
        return Err(ApiError::BadRequest("The signature is too long".into()));
    }
    let email_signature = Field::from_input(input.email_signature.as_ref())
        .map(|html| Some(email_signature::sanitize(html)).filter(|clean| !clean.is_empty()));
    let cloudtalk_agent_id = Field::from_input(input.cloudtalk_agent_id.as_ref());
    if let Some(agent) = cloudtalk_agent_id.value() {
        digits_field(Some(agent), "CloudTalk agent ID", 1)?;
        if agent.len() > 36 {
            return Err(ApiError::BadRequest(
                "CloudTalk agent ID must be at most 36 characters".into(),
            ));
        }
    }
    let cloudtalk_phone = Field::from_input(input.cloudtalk_phone_number.as_ref());
    digits_field(cloudtalk_phone.value(), "CloudTalk phone", 10)?;
    let ringcentral_phone = Field::from_input(input.ringcentral_phone_number.as_ref());
    digits_field(ringcentral_phone.value(), "RingCentral phone", 10)?;
    Ok(Checked {
        name,
        email,
        phone_number,
        password,
        email_name: Field::from_input(input.email_name.as_ref()),
        email_signature,
        cloudtalk_agent_id,
        cloudtalk_phone: cloudtalk_phone.map(|p| line10(Some(p))),
        ringcentral_phone: ringcentral_phone.map(|p| line10(Some(p))),
    })
}

/// How a submitted line compares with the stored one, by rank.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum LineStatus {
    None,
    Incomplete,
    Unmatched,
    Unchanged,
    Saved,
}

impl LineStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Incomplete => "incomplete",
            Self::Unmatched => "unmatched",
            Self::Unchanged => "unchanged",
            Self::Saved => "saved",
        }
    }
}

/// A `CloudTalk` line (`resolveCloudtalkStatus`): it needs its agent id, is
/// unchanged when both match what is stored, and otherwise needs the
/// company's `CloudTalk` connection.
fn cloudtalk_status(
    phone: Option<&str>,
    agent: Option<&str>,
    stored: &Stored,
    connected: bool,
) -> LineStatus {
    let Some(phone) = phone else {
        return LineStatus::None;
    };
    let Some(agent) = agent else {
        return LineStatus::Incomplete;
    };
    if Some(phone) == line10(stored.cloudtalk_phone_number.as_deref()).as_deref()
        && stored.cloudtalk_agent_id.as_deref() == Some(agent)
    {
        return LineStatus::Unchanged;
    }
    if connected {
        LineStatus::Saved
    } else {
        LineStatus::Unmatched
    }
}

/// A `RingCentral` line (`resolveRingcentralStatus`): saved when `RingCentral`
/// found a new number, unchanged when it is the stored one.
fn ringcentral_status(
    phone: Option<&str>,
    extension: Option<&str>,
    matched: bool,
    stored: &Stored,
) -> LineStatus {
    let Some(phone) = phone else {
        return LineStatus::None;
    };
    let same = Some(phone) == line10(stored.ringcentral_phone_number.as_deref()).as_deref();
    if matched {
        if !same || stored.ringcentral_extension_id.is_none() {
            LineStatus::Saved
        } else {
            LineStatus::Unchanged
        }
    } else if same && extension.is_some() {
        LineStatus::Unchanged
    } else {
        LineStatus::Unmatched
    }
}

/// The texting line a save submits, resolved with its provider.
struct TextingLine {
    status: LineStatus,
    provider: &'static str,
    /// The `RingCentral` extension to store.
    extension: Option<String>,
    /// `RingCentral` found the submitted number.
    matched: bool,
}

async fn texting_line(
    state: &AppState,
    actor: store::Actor,
    input: &Checked,
    before: &Stored,
) -> ApiResult<TextingLine> {
    let (extension, matched) = if input.ringcentral_phone.submitted() {
        let link = resolve_ringcentral_extension(
            state,
            actor.company_id,
            actor.user_id,
            input.ringcentral_phone.value(),
        )
        .await?;
        (link.extension_id, link.matched)
    } else {
        (before.ringcentral_extension_id.clone(), false)
    };
    let phone = input.cloudtalk_phone.value();
    let agent = input.cloudtalk_agent_id.value();
    let connected = phone.is_some()
        && agent.is_some()
        && company_features(&state.pool, actor.company_id)
            .await?
            .cloudtalk;
    let ct = cloudtalk_status(phone, agent, before, connected);
    let rc = ringcentral_status(
        input.ringcentral_phone.value(),
        extension.as_deref(),
        matched,
        before,
    );
    // CloudTalk wins a tie: submitting both at once is not a supported flow.
    let (status, provider) = if rc > ct {
        (rc, "RingCentral")
    } else {
        (ct, "CloudTalk")
    };
    Ok(TextingLine {
        status,
        provider,
        extension,
        matched,
    })
}

/// Saves the row; a new password signs the user out everywhere else.
async fn save(
    state: &AppState,
    actor: store::Actor,
    headers: &HeaderMap,
    input: &Checked,
    before: &Stored,
    extension: Option<&String>,
) -> ApiResult<()> {
    let password = match input.password.clone() {
        Some(password) => Some(
            tokio::task::spawn_blocking(move || bcrypt::hash(password, BCRYPT_COST))
                .await
                .map_err(|err| ApiError::Internal(err.to_string()))?
                .map_err(|err| ApiError::Internal(err.to_string()))?,
        ),
        None => None,
    };
    let mut tx = state.pool.begin().await?;
    let updated = sqlx::query(
        "UPDATE users SET name = ?, email = ?, phone_number = ?, email_name = ?,
                email_signature = ?, cloudtalk_agent_id = ?, cloudtalk_phone_number = ?,
                ringcentral_extension_id = ?, ringcentral_phone_number = ?,
                password = COALESCE(?, password)
          WHERE id = ? AND company_id = ? AND is_deleted = 0",
    )
    .bind(&input.name)
    .bind(&input.email)
    .bind(input.phone_number.or_kept(before.phone_number.as_ref()))
    .bind(input.email_name.or_kept(before.email_name.as_ref()))
    .bind(
        input
            .email_signature
            .or_kept(before.email_signature.as_ref()),
    )
    .bind(
        input
            .cloudtalk_agent_id
            .or_kept(before.cloudtalk_agent_id.as_ref()),
    )
    .bind(
        input
            .cloudtalk_phone
            .or_kept(before.cloudtalk_phone_number.as_ref()),
    )
    .bind(extension)
    .bind(
        input
            .ringcentral_phone
            .or_kept(before.ringcentral_phone_number.as_ref()),
    )
    .bind(&password)
    .bind(actor.user_id)
    .bind(actor.company_id)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(ApiError::Unauthenticated("Sign in again.".into()));
    }
    if password.is_some() {
        let current = request_session_id(headers, &state.session_secret).unwrap_or_default();
        sqlx::query(
            "UPDATE sessions SET is_deleted = 1 WHERE user_id = ? AND id <> ? AND is_deleted = 0",
        )
        .bind(actor.user_id)
        .bind(current)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// What this save finished (empty before, filled now) pays its setup task.
async fn finished_setup_credit(
    state: &AppState,
    actor: store::Actor,
    input: &Checked,
    before: &Stored,
) -> Option<Credit> {
    let mut finished = Vec::new();
    let empty_before = |value: Option<&String>| value.is_none_or(|v| v.trim().is_empty());
    if input.email_name.value().is_some() && empty_before(before.email_name.as_ref()) {
        finished.push("setup-email-name");
    }
    if input.email_signature.value().is_some()
        && before
            .email_signature
            .as_deref()
            .is_none_or(|s| email_signature::sanitize(s).is_empty())
    {
        finished.push("setup-email-signature");
    }
    if finished.is_empty() {
        return None;
    }
    match onboarding::credit_done(state, actor.user_id, &finished).await {
        Ok(CreditOutcome::Applied {
            credited, state, ..
        }) if !credited.is_empty() => Some(Credit {
            task_ids: credited,
            state,
        }),
        // Best effort: the save stands without the reward.
        Ok(_) => None,
        Err(error) => {
            tracing::error!(user_id = actor.user_id, ?error, "setup credit failed");
            None
        }
    }
}

/// Save my account
///
/// Saves the user's details in their home company. A new password ends the
/// user's other sessions and signs out their Granite Phone apps. The texting line is checked with its provider (the
/// `RingCentral` extension is looked up from the number), and a number moved
/// from `CloudTalk` to `RingCentral` brings its texting history along. A newly
/// filled email name or signature completes its setup task, with its points.
#[crate::openapi::path(
    put,
    path = "/v1/me/account",
    operation_id = "update_my_account",
    tag = "me",
    request_body = AccountInput,
    responses(
        (status = 200, description = "Saved", body = AccountSaved),
        (status = 400, description = "A field is invalid", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or of the shop", body = ErrorBody),
        (status = 409, description = "The email belongs to another account", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_account(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    headers: HeaderMap,
    JsonBody(input): JsonBody<AccountInput>,
) -> ApiResult<Json<AccountSaved>> {
    let input = check(&input)?;
    let pool = &state.pool;
    let actor = home_actor(pool, user.id).await?;
    let before = stored(pool, actor).await?;
    if before.email.as_deref().map(str::to_lowercase).as_deref() != Some(input.email.as_str()) {
        let taken: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM users WHERE LOWER(email) = ? AND id <> ? AND is_deleted = 0)",
        )
        .bind(&input.email)
        .bind(actor.user_id)
        .fetch_one(pool)
        .await?;
        if taken {
            return Err(ApiError::Conflict(
                "This email belongs to another account".into(),
            ));
        }
    }
    let line = texting_line(&state, actor, &input, &before).await?;
    save(
        &state,
        actor,
        &headers,
        &input,
        &before,
        line.extension.as_ref(),
    )
    .await?;
    if input.password.is_some() {
        crate::phone::revoke::revoke_user_phones(
            &state,
            actor.user_id,
            crate::phone::revoke::RevokeReason::PasswordChanged,
        )
        .await?;
    }
    texting_history::follow(
        &state,
        actor.company_id,
        actor.user_id,
        &before.line(),
        &texting_history::Line {
            cloudtalk_agent: input
                .cloudtalk_agent_id
                .or_kept(before.cloudtalk_agent_id.as_ref()),
            cloudtalk_phone: line10(
                input
                    .cloudtalk_phone
                    .or_kept(before.cloudtalk_phone_number.as_ref())
                    .as_deref(),
            ),
            ringcentral_extension: line.extension.clone(),
            ringcentral_phone: line10(
                input
                    .ringcentral_phone
                    .or_kept(before.ringcentral_phone_number.as_ref())
                    .as_deref(),
            ),
        },
        line.matched,
    )
    .await;
    let credit = finished_setup_credit(&state, actor, &input, &before).await;
    Ok(Json(AccountSaved {
        texting_line_status: line.status.as_str().into(),
        texting_line_provider: (line.status != LineStatus::None).then(|| line.provider.into()),
        credit,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emails_are_single_plain_addresses() {
        for ok in [
            "rep@acme.com",
            "first.last+tag@mail.acme.co",
            "o'neil_x@acme-stone.com",
        ] {
            assert!(valid_email(ok), "{ok}");
        }
        for bad in [
            "%@%.%",
            "rep%@acme.com",
            "rep@ac%me.com",
            "a@b.com, c@d.com",
            "Rep <rep@acme.com>",
            "rep@acme_x.com",
            "rep@acme",
            "rep@@acme.com",
            "rep @acme.com",
        ] {
            assert!(!valid_email(bad), "{bad}");
        }
    }

    fn stored_line(
        ct: Option<&str>,
        agent: Option<&str>,
        rc: Option<&str>,
        ext: Option<&str>,
    ) -> Stored {
        Stored {
            cloudtalk_phone_number: ct.map(str::to_string),
            cloudtalk_agent_id: agent.map(str::to_string),
            ringcentral_phone_number: rc.map(str::to_string),
            ringcentral_extension_id: ext.map(str::to_string),
            ..Stored::default()
        }
    }

    #[test]
    fn cloudtalk_lines() {
        let s = stored_line(Some("5551234567"), Some("42"), None, None);
        let p = Some("5551234567");
        assert_eq!(
            cloudtalk_status(None, Some("42"), &s, true),
            LineStatus::None
        );
        assert_eq!(cloudtalk_status(p, None, &s, true), LineStatus::Incomplete);
        assert_eq!(
            cloudtalk_status(p, Some("42"), &s, false),
            LineStatus::Unchanged
        );
        assert_eq!(
            cloudtalk_status(p, Some("43"), &s, false),
            LineStatus::Unmatched
        );
        assert_eq!(cloudtalk_status(p, Some("43"), &s, true), LineStatus::Saved);
    }

    #[test]
    fn ringcentral_lines() {
        let s = stored_line(None, None, Some("5551234567"), Some("9"));
        let p = Some("5551234567");
        assert_eq!(
            ringcentral_status(p, Some("9"), true, &s),
            LineStatus::Unchanged
        );
        assert_eq!(
            ringcentral_status(Some("5550000000"), Some("8"), true, &s),
            LineStatus::Saved
        );
        assert_eq!(
            ringcentral_status(p, Some("9"), false, &s),
            LineStatus::Unchanged
        );
        assert_eq!(
            ringcentral_status(Some("5550000000"), Some("9"), false, &s),
            LineStatus::Unmatched
        );
    }
}
