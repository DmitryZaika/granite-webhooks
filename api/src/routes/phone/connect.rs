//! "Connect Telnyx": a company sets itself up for Granite Phone from its own
//! Phone page, and buys numbers there. Connecting makes (or, run again,
//! re-points at us) everything the company needs in our Telnyx account: a
//! billing group (its costs apart), an outbound voice profile (US and
//! Canada, a daily spend limit), the TeXML application its numbers ring
//! through, the credential connection its phones register on (with the
//! app's push credentials), and a messaging profile for texting; then it
//! puts the company's numbers on them and imports the TeXML app's numbers.
//! Each piece is saved as soon as Telnyx makes it, so a failed connect
//! continues where it stopped when tried again.

use super::admin::{AdminNumber, company_number, import_numbers};
use super::superuser::{TelnyxCompanyConfig, new_webhook_token};
use super::{audit, telnyx_failed};
use crate::auth::{AdminUser, CurrentUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::phone::company;
use crate::phone::numbers::{is_nanp, last10, to_e164};
use crate::state::AppState;
use crate::telnyx::TelnyxError;
use crate::telnyx::provision::{
    AvailableNumber, CredentialConnectionSpec, MessagingProfileSpec, NAME_TAKEN, PushCredentials,
    TexmlAppSpec, VoiceProfileSpec,
};
use axum::Json;
use axum::extract::{Path, Query, State};
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::MySqlPool;
use utoipa::{IntoParams, ToSchema};

/// A new company's daily spend limits in Telnyx, in dollars (calls, texts).
const DAILY_CALL_SPEND: &str = "25.00";
const DAILY_TEXT_SPEND: &str = "25.00";
/// Most numbers a company may hold; a superuser buys past it.
const MAX_COMPANY_NUMBERS: i64 = 25;
/// Numbers one search shows.
const SEARCH_LIMIT: u32 = 12;

/// What connecting did.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TelnyxConnection {
    /// Calls can ring and be placed: the TeXML app and the credential
    /// connection are set.
    pub configured: bool,
    /// What Telnyx made this time, of `billingGroup`, `voiceProfile`,
    /// `texmlApp`, `credentialConnection` and `messagingProfile`; empty when
    /// everything was there already and was only pointed at us again.
    pub created: Vec<String>,
    /// Numbers added from the TeXML app.
    pub imported: usize,
}

/// The company's slug in Telnyx names: `gm-{id}-{name}` (lowercase letters,
/// digits and dashes; the name cut to 40 characters).
fn resource_name(company_id: i32, company_name: &str) -> String {
    let lowered = company_name.to_lowercase();
    let words: Vec<&str> = lowered
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    let slug: String = words.join("-").chars().take(40).collect();
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        format!("gm-{company_id}")
    } else {
        format!("gm-{company_id}-{slug}")
    }
}

/// Four random letters and digits, to make a taken name unique.
fn name_suffix() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = [0u8; 4];
    if getrandom::fill(&mut bytes).is_err() {
        return "x".repeat(4);
    }
    bytes
        .iter()
        .map(|byte| char::from(ALPHABET[usize::from(*byte) % ALPHABET.len()]))
        .collect()
}

/// `$create` (a Telnyx call naming the resource `$name: &str`), awaited,
/// and once more with a random ending when Telnyx says the name is taken
/// (an old resource, another environment). A macro, not a function taking
/// an async closure: those futures are not provably `Send` for axum.
macro_rules! create_unique {
    ($base:expr, |$name:ident| $create:expr) => {{
        let base: String = $base;
        let first: Result<String, TelnyxError> = {
            let $name: &str = &base;
            $create.await
        };
        match first {
            Err(err) if err.code() == Some(NAME_TAKEN) => {
                let retry = format!("{base}-{}", name_suffix());
                let $name: &str = &retry;
                $create.await
            }
            other => other,
        }
    }};
}

/// Saves one Telnyx id of the company's setup (`column` is one of ours).
async fn save_id(
    pool: &MySqlPool,
    company_id: i32,
    column: &'static str,
    id: &str,
) -> sqlx::Result<()> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE telnyx_company_config SET {column} = ? WHERE company_id = ?"
    )))
    .bind(id)
    .bind(company_id)
    .execute(pool)
    .await
    .map(drop)
}

/// Connects the company (see the module): one connect at a time per
/// company, 404 for an unknown or deleted company, 409 while another
/// connect runs, 503 without a Telnyx key, 502 when Telnyx fails (what was
/// made is kept; try again).
pub(super) async fn connect_company(
    state: &AppState,
    company_id: i32,
    actor: i32,
) -> ApiResult<TelnyxConnection> {
    let name: Option<String> =
        sqlx::query_scalar("SELECT name FROM company WHERE id = ? AND deleted_at IS NULL")
            .bind(company_id)
            .fetch_optional(&state.pool)
            .await?;
    let name = name.ok_or(ApiError::NotFound)?;
    if !state.telnyx.is_enabled() {
        return Err(ApiError::Unavailable(
            "Calling is not set up on the server.".into(),
        ));
    }
    // A second click while Telnyx is still answering would make everything
    // twice. The lock is the connection's: it goes if the Lambda dies. Lock
    // names are the MySQL server's, so the database's name is in it (the
    // production and dev databases share a server).
    let lock = format!("telnyx-connect-{company_id}");
    let mut connection = state.pool.acquire().await?;
    let locked: Option<i64> =
        sqlx::query_scalar("SELECT GET_LOCK(LEFT(CONCAT(DATABASE(), ':', ?), 64), 0)")
            .bind(&lock)
            .fetch_one(&mut *connection)
            .await?;
    if locked != Some(1) {
        return Err(ApiError::Conflict(
            "Telnyx is being connected already. Try again in a minute.".into(),
        ));
    }
    let result = provision(state, company_id, &name, actor).await;
    if let Err(err) = sqlx::query("DO RELEASE_LOCK(LEFT(CONCAT(DATABASE(), ':', ?), 64))")
        .bind(&lock)
        .execute(&mut *connection)
        .await
    {
        tracing::warn!(%err, company_id, "could not release the Telnyx connect lock");
    }
    result
}

async fn provision(
    state: &AppState,
    company_id: i32,
    company_name: &str,
    actor: i32,
) -> ApiResult<TelnyxConnection> {
    let pool = &state.pool;
    let telnyx = &state.telnyx;
    // A company connecting for the first time gets calling switched on; a
    // superuser's switch-off stays.
    sqlx::query(
        "INSERT IGNORE INTO telnyx_company_config (company_id, webhook_token, enabled)
         VALUES (?, ?, 1)",
    )
    .bind(company_id)
    .bind(new_webhook_token()?)
    .execute(pool)
    .await?;
    let config = company::load(pool, company_id)
        .await?
        .ok_or_else(|| ApiError::Internal("the Telnyx setup was not saved".into()))?;
    let base = config.webhook_base(&state.phone);
    let name = resource_name(company_id, company_name);
    let mut created: Vec<String> = Vec::new();

    let billing_group = if let Some(id) = config.billing_group_id.clone() {
        id
    } else {
        let label = format!("Granite Manager: {} (#{company_id})", company_name.trim());
        let id = create_unique!(label, |group| telnyx.create_billing_group(group))
            .map_err(|err| telnyx_failed(&err, "create the billing group"))?;
        save_id(pool, company_id, "billing_group_id", &id).await?;
        created.push("billingGroup".into());
        id
    };

    let voice_profile = if let Some(id) = config.outbound_voice_profile_id.clone() {
        id
    } else {
        let id = create_unique!(name.clone(), |profile| telnyx.create_voice_profile(
            &VoiceProfileSpec {
                name: profile,
                billing_group_id: Some(&billing_group),
                daily_spend_limit: DAILY_CALL_SPEND,
            }
        ))
        .map_err(|err| telnyx_failed(&err, "create the outbound voice profile"))?;
        save_id(pool, company_id, "outbound_voice_profile_id", &id).await?;
        created.push("voiceProfile".into());
        id
    };

    let fallback = config.fallback_texml_url.clone().or_else(|| {
        state
            .phone
            .fallback_texml_url
            .as_deref()
            .map(str::to_string)
    });
    let voice_url = format!("{base}/inbound");
    let status_callback = format!("{base}/call-status");
    let mut texml_app = config.texml_app_id.clone();
    if let Some(id) = &texml_app {
        // The name is left as it is.
        let spec = TexmlAppSpec {
            name: "",
            voice_url: &voice_url,
            status_callback: &status_callback,
            fallback_url: fallback.as_deref(),
            outbound_voice_profile_id: &voice_profile,
        };
        match telnyx.update_texml_app(id, &spec).await {
            Ok(()) => {}
            // Deleted in Telnyx: make a new one.
            Err(err) if err.status() == Some(404) => texml_app = None,
            Err(err) => return Err(telnyx_failed(&err, "update the TeXML application")),
        }
    }
    let texml_app = if let Some(id) = texml_app {
        id
    } else {
        let id = create_unique!(name.clone(), |app| telnyx.create_texml_app(&TexmlAppSpec {
            name: app,
            voice_url: &voice_url,
            status_callback: &status_callback,
            fallback_url: fallback.as_deref(),
            outbound_voice_profile_id: &voice_profile,
        }))
        .map_err(|err| telnyx_failed(&err, "create the TeXML application"))?;
        save_id(pool, company_id, "texml_app_id", &id).await?;
        created.push("texmlApp".into());
        id
    };

    // Without the app's push credentials, phones ring only while the app is
    // open; connecting still goes on.
    let push = telnyx.push_credentials().await.unwrap_or_else(|err| {
        tracing::warn!(%err, company_id, "could not read the push credentials");
        PushCredentials::default()
    });
    let outbound_url = format!("{base}/outbound");
    let connection_name = format!("{name}-phones");
    let mut credential_connection = config.credential_connection_id.clone();
    if let Some(id) = &credential_connection {
        // The name is left as it is.
        let spec = CredentialConnectionSpec {
            name: "",
            webhook_url: &outbound_url,
            outbound_voice_profile_id: &voice_profile,
            android_push_credential_id: push.android.as_deref(),
            ios_push_credential_id: push.ios.as_deref(),
        };
        match telnyx.update_credential_connection(id, &spec).await {
            Ok(()) => {}
            Err(err) if err.status() == Some(404) => credential_connection = None,
            Err(err) => return Err(telnyx_failed(&err, "update the credential connection")),
        }
    }
    if credential_connection.is_none() {
        let id = create_unique!(connection_name, |connection| telnyx
            .create_credential_connection(&CredentialConnectionSpec {
                name: connection,
                webhook_url: &outbound_url,
                outbound_voice_profile_id: &voice_profile,
                android_push_credential_id: push.android.as_deref(),
                ios_push_credential_id: push.ios.as_deref(),
            }))
        .map_err(|err| telnyx_failed(&err, "create the credential connection"))?;
        save_id(pool, company_id, "credential_connection_id", &id).await?;
        created.push("credentialConnection".into());
    }

    let messaging_url = format!("{base}/messaging");
    let mut messaging_profile = config.messaging_profile_id.clone();
    if let Some(id) = &messaging_profile {
        match telnyx.update_messaging_webhook(id, &messaging_url).await {
            Ok(()) => {}
            Err(err) if err.status() == Some(404) => messaging_profile = None,
            Err(err) => return Err(telnyx_failed(&err, "update the messaging profile")),
        }
    }
    let messaging_profile = if let Some(id) = messaging_profile {
        id
    } else {
        let id = create_unique!(name.clone(), |profile| telnyx.create_messaging_profile(
            &MessagingProfileSpec {
                name: profile,
                webhook_url: &messaging_url,
                daily_spend_limit: DAILY_TEXT_SPEND,
            }
        ))
        .map_err(|err| telnyx_failed(&err, "create the messaging profile"))?;
        save_id(pool, company_id, "messaging_profile_id", &id).await?;
        created.push("messagingProfile".into());
        id
    };

    // The company's numbers already on our account (bought, ported, or
    // imported before) ring through the app and text through the profile.
    let numbers: Vec<String> = sqlx::query_scalar(
        "SELECT telnyx_number_id FROM telnyx_numbers
          WHERE company_id = ? AND telnyx_number_id IS NOT NULL AND active <> 0",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    for number in &numbers {
        if let Err(err) = telnyx
            .assign_number(number, &texml_app, Some(&billing_group))
            .await
        {
            tracing::warn!(%err, company_id, "could not put a number on the TeXML app");
        }
        if let Err(err) = telnyx
            .set_number_messaging(number, &messaging_profile)
            .await
        {
            tracing::warn!(%err, company_id, "could not give a number the messaging profile");
        }
    }
    let imported = import_numbers(state, company_id, actor, &texml_app).await?;
    audit(
        pool,
        company_id,
        actor,
        "telnyx_connected",
        &format!("company:{company_id}"),
        &json!({ "created": created, "imported": imported }),
    )
    .await?;
    Ok(TelnyxConnection {
        configured: true,
        created,
        imported,
    })
}

/// Connect Telnyx
///
/// Sets the caller's company up for Granite Phone calls and texts in one
/// go (see the module): billing group, outbound voice profile, TeXML
/// application, credential connection and messaging profile, each made
/// when missing and pointed at the company's webhooks; the company's
/// numbers are put on them and the TeXML app's numbers imported. Safe to
/// run again: it repairs what changed in Telnyx. A company connecting for
/// the first time gets calling switched on. 409 while another connect of
/// the company runs, 502 when Telnyx fails (what was made is kept; try
/// again), 503 when Telnyx is not set up on the server. Written to the
/// phone audit log. Company admins (and super admins, superusers) only.
#[utoipa::path(
    post,
    path = "/v1/phone/admin/connect",
    operation_id = "connect_phone_admin_telnyx",
    tag = "phone",
    responses(
        (status = 200, description = "Connected", body = TelnyxConnection),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 409, description = "Another connect is running", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "Telnyx failed; try again", body = ErrorBody),
        (status = 503, description = "Telnyx is not set up on the server", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn connect_telnyx(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<TelnyxConnection>> {
    Ok(Json(
        connect_company(&state, user.company_id, user.id).await?,
    ))
}

/// Connect a company's Telnyx
///
/// As `POST /v1/phone/admin/connect`, for any company; answers the
/// company's whole setup. 404 for an unknown or deleted company.
/// Superusers only.
#[utoipa::path(
    post,
    path = "/v1/superuser/companies/{id}/telnyx/connect",
    operation_id = "connect_company_telnyx",
    tag = "phone",
    params(("id" = i32, Path, description = "Company id")),
    responses(
        (status = 200, description = "The setup after connecting", body = TelnyxCompanyConfig),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "No such company", body = ErrorBody),
        (status = 409, description = "Another connect is running", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "Telnyx failed; try again", body = ErrorBody),
        (status = 503, description = "Telnyx is not set up on the server", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn connect_company_telnyx(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<TelnyxCompanyConfig>> {
    if !user.is_superuser {
        return Err(ApiError::Forbidden);
    }
    connect_company(&state, id, user.id).await?;
    let config = company::load(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(TelnyxCompanyConfig::of(&config, &state)))
}

/// `GET /v1/phone/admin/numbers/available`.
#[derive(Debug, Deserialize, IntoParams)]
#[serde(rename_all = "camelCase")]
#[into_params(parameter_in = Query)]
pub struct NumberSearch {
    /// A US area code, like `317`.
    pub area_code: String,
}

/// A number the company can buy.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BuyableNumber {
    /// E.164, like `+13175550101`.
    pub e164: String,
    /// Its town, when Telnyx gives one.
    pub locality: Option<String>,
    /// Its state, like `IN`.
    pub region: Option<String>,
    /// Dollars a month (`"1.00"`), when Telnyx gives it.
    pub monthly_cost: Option<String>,
    /// Dollars up front, when Telnyx gives it.
    pub upfront_cost: Option<String>,
}

/// Numbers for sale in an area code.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BuyableNumbers {
    /// At most 12, in Telnyx's order.
    pub numbers: Vec<BuyableNumber>,
}

/// Dollars to two places (`"1.00000"` → `"1.00"`); other text as it is.
fn dollars(amount: Option<String>) -> Option<String> {
    amount.map(|amount| {
        amount
            .parse::<f64>()
            .map_or(amount, |value| format!("{value:.2}"))
    })
}

impl From<AvailableNumber> for BuyableNumber {
    fn from(number: AvailableNumber) -> Self {
        Self {
            e164: number.e164,
            locality: number.locality,
            region: number.region,
            monthly_cost: dollars(number.monthly_cost),
            upfront_cost: dollars(number.upfront_cost),
        }
    }
}

/// Find numbers to buy
///
/// Local US numbers with calls and texts for sale in an area code (at most
/// 12), with their monthly and up-front price. 400 unless `areaCode` is
/// three digits, 502 when Telnyx fails, 503 when Telnyx is not set up on
/// the server. Company admins (and super admins, superusers) only.
#[utoipa::path(
    get,
    path = "/v1/phone/admin/numbers/available",
    operation_id = "search_phone_admin_numbers",
    tag = "phone",
    params(NumberSearch),
    responses(
        (status = 200, description = "Numbers for sale", body = BuyableNumbers),
        (status = 400, description = "Not an area code", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 502, description = "Telnyx failed; try again", body = ErrorBody),
        (status = 503, description = "Telnyx is not set up on the server", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn search_numbers(
    State(state): State<AppState>,
    AdminUser(_user): AdminUser,
    Query(search): Query<NumberSearch>,
) -> ApiResult<Json<BuyableNumbers>> {
    let area_code = search.area_code.trim();
    let valid = area_code.len() == 3
        && area_code.chars().all(|c| c.is_ascii_digit())
        && !area_code.starts_with(['0', '1']);
    if !valid {
        return Err(ApiError::BadRequest(
            "Enter a three-digit area code, like 317.".into(),
        ));
    }
    let numbers = state
        .telnyx
        .search_numbers(area_code, SEARCH_LIMIT)
        .await
        .map_err(|err| telnyx_failed(&err, "search numbers"))?;
    Ok(Json(BuyableNumbers {
        numbers: numbers.into_iter().map(BuyableNumber::from).collect(),
    }))
}

/// Body of `POST /v1/phone/admin/numbers/buy`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BuyNumber {
    /// The number to buy, from `GET /v1/phone/admin/numbers/available`.
    pub e164: String,
}

/// Buy a number
///
/// Buys a number for the caller's company in Telnyx (on its TeXML app,
/// messaging profile and billing group) and adds it as an unassigned direct
/// line; answers it (assign it with `PUT /v1/phone/admin/numbers/{id}`).
/// The company is charged by Telnyx's price for it. 400 when Telnyx is not
/// connected for the company or the number is not a US or Canadian one,
/// 403 while its calling is switched off or once it has 25 numbers (a
/// superuser can buy either way), 409 when
/// the company has it already, 502 when Telnyx fails (the number may be
/// taken: search again). Written to the phone audit log. Company admins
/// (and super admins, superusers) only.
#[utoipa::path(
    post,
    path = "/v1/phone/admin/numbers/buy",
    operation_id = "buy_phone_admin_number",
    tag = "phone",
    request_body = BuyNumber,
    responses(
        (status = 200, description = "The number, bought", body = AdminNumber),
        (status = 400, description = "Not connected, or not a number", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not an admin, or the company has its most numbers", body = ErrorBody),
        (status = 409, description = "The company has the number already", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "Telnyx did not sell it; try another", body = ErrorBody),
        (status = 503, description = "Telnyx is not set up on the server", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn buy_number(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<BuyNumber>,
) -> ApiResult<Json<AdminNumber>> {
    let pool = &state.pool;
    let e164 = to_e164(&input.e164)
        .filter(|e164| is_nanp(e164))
        .ok_or_else(|| ApiError::BadRequest("Pick a US or Canadian number.".into()))?;
    let digits = last10(&e164)
        .ok_or_else(|| ApiError::BadRequest("Pick a US or Canadian number.".into()))?;
    let Some((config, texml_app)) =
        company::load(pool, user.company_id)
            .await?
            .and_then(|config| {
                let app = config.texml_app_id.clone()?;
                Some((config, app))
            })
    else {
        return Err(ApiError::BadRequest(
            "Connect Telnyx first, then buy numbers.".into(),
        ));
    };
    // Granite Manager switched the company's calling off: no new numbers.
    if !config.enabled && !user.is_superuser {
        return Err(ApiError::Denied(
            "Calling is turned off for your company. Contact Granite Manager support.".into(),
        ));
    }
    let held: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM telnyx_numbers WHERE company_id = ? AND active <> 0",
    )
    .bind(user.company_id)
    .fetch_one(pool)
    .await?;
    if held >= MAX_COMPANY_NUMBERS && !user.is_superuser {
        return Err(ApiError::Denied(format!(
            "Your company has {MAX_COMPANY_NUMBERS} numbers, the most it can buy here. Ask the Granite Manager team for more."
        )));
    }
    let owner: Option<i32> =
        sqlx::query_scalar("SELECT company_id FROM telnyx_numbers WHERE e164 = ?")
            .bind(&e164)
            .fetch_optional(pool)
            .await?;
    if owner.is_some() {
        return Err(ApiError::Conflict(
            "That number is in Granite Manager already.".into(),
        ));
    }
    let order = state
        .telnyx
        .order_number(
            &e164,
            &texml_app,
            config.messaging_profile_id.as_deref(),
            config.billing_group_id.as_deref(),
            &format!("company-{}", user.company_id),
        )
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "Telnyx number order failed");
            match err {
                TelnyxError::NotConfigured => {
                    ApiError::Unavailable("Calling is not set up on the server.".into())
                }
                _ => ApiError::Upstream(
                    "Telnyx could not sell that number. It may be taken: search again.".into(),
                ),
            }
        })?;
    if order.status == "failure" {
        return Err(ApiError::Upstream(
            "Telnyx could not sell that number. It may be taken: search again.".into(),
        ));
    }
    let mut tx = pool.begin().await?;
    let inserted = sqlx::query(
        "INSERT IGNORE INTO telnyx_numbers (company_id, e164, last10, label, kind)
         VALUES (?, ?, ?, '', 'direct')",
    )
    .bind(user.company_id)
    .bind(&e164)
    .bind(&digits)
    .execute(&mut *tx)
    .await?;
    let id: Option<i32> =
        sqlx::query_scalar("SELECT id FROM telnyx_numbers WHERE e164 = ? AND company_id = ?")
            .bind(&e164)
            .bind(user.company_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(id) = id else {
        return Err(ApiError::Conflict(
            "That number is in Granite Manager already.".into(),
        ));
    };
    audit(
        &mut *tx,
        user.company_id,
        user.id,
        "number_bought",
        &format!("number:{id}"),
        &json!({ "e164": e164, "orderId": order.id, "status": order.status,
                 "added": inserted.rows_affected() == 1 }),
    )
    .await?;
    tx.commit().await?;
    let number = company_number(pool, user.company_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(number))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_carry_the_company_id_and_a_short_slug() {
        assert_eq!(
            resource_name(1, "Granite Depot of Indianapolis"),
            "gm-1-granite-depot-of-indianapolis"
        );
        assert_eq!(resource_name(7, "  Éclat & Co. "), "gm-7-clat-co");
        assert_eq!(resource_name(9, "!!!"), "gm-9");
        let long = resource_name(3, &"stone ".repeat(20));
        assert!(long.len() <= "gm-3-".len() + 40, "{long}");
        assert!(!long.ends_with('-'));
    }

    #[test]
    fn prices_show_in_cents() {
        assert_eq!(dollars(Some("1.00000".into())).as_deref(), Some("1.00"));
        assert_eq!(dollars(Some("0.5".into())).as_deref(), Some("0.50"));
        assert_eq!(dollars(Some("n/a".into())).as_deref(), Some("n/a"));
        assert_eq!(dollars(None), None);
    }
}
