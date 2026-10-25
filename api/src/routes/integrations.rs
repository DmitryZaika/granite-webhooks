//! Company settings (`/admin/company`) and connecting `QuickBooks` and
//! `RingCentral`.
//!
//! - `GET  /v1/company/settings`                  → name, address, logo, sales tax, integrations
//! - `PUT  /v1/company/settings`                  → save them
//! - `POST /v1/company/api-token-email`           → email the docs API bearer token (managers)
//! - `GET  /v1/quickbooks/connection`             → can the company connect, is it connected
//! - `POST /v1/quickbooks/connection/start`       → Intuit's consent link
//! - `POST /v1/quickbooks/connection`             → finish: the code Intuit returned
//! - `DELETE /v1/quickbooks/connection`           → disconnect
//! - `POST /v1/quickbooks/disconnected`           → Intuit's disconnect link (signed in)
//! - `PUT  /v1/superuser/companies/{id}/quickbooks-app` → a company's own app keys
//! - `POST /v1/ringcentral/connection/start`      → `RingCentral`'s sign-in link
//! - `POST /v1/ringcentral/connection`            → finish: the code `RingCentral` returned
//! - `DELETE /v1/ringcentral/connection`          → disconnect
//!
//! The providers return to the app's pages (`/redirects/quickbooks/user-auth`,
//! `/redirects/ringcentral/user-auth` on `APP_URL`, the redirect URIs
//! registered with them), which post the code here. Replaces the Remix
//! `admin.company`, `redirects.*`, `quickbooks.disconnect` and
//! `superuser.add-qbo` routes.

use crate::auth::{EmployeeUser, UserManager};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::oauth_state;
use crate::openapi::{IntoParams, ToSchema};
use crate::openapi::{OpenApiRouter, routes};
use crate::qbo::{self, CredentialsSource, QboError};
use crate::ringcentral_connect::{self, ConnectionStatus};
use crate::routes::auth::company_subdomain;
use crate::routes::uploads::{self, schemas::UploadFolder};
use crate::state::AppState;
use crate::webhooks_client::{OutgoingEmail, SendFailure};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(get_settings, save_settings))
        .routes(routes!(email_api_token))
        .routes(routes!(
            quickbooks_status,
            finish_quickbooks,
            disconnect_quickbooks
        ))
        .routes(routes!(start_quickbooks))
        .routes(routes!(quickbooks_disconnected))
        .routes(routes!(save_quickbooks_app))
        .routes(routes!(finish_ringcentral, disconnect_ringcentral))
        .routes(routes!(start_ringcentral))
}

const MANAGER_POSITION: i32 = 5;
const QBO_RETURN_PATH: &str = "/redirects/quickbooks/user-auth";
const RC_RETURN_PATH: &str = "/redirects/ringcentral/user-auth";

/// `?company_id=`: a superuser works on another company.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TargetCompany {
    /// Superusers only: the company to act on (default: the caller's).
    #[serde(default)]
    pub company_id: Option<i32>,
}

const fn target(user: &crate::auth::session::SessionUser, query: &TargetCompany) -> i32 {
    match query.company_id {
        Some(id) if user.is_superuser => id,
        _ => user.company_id,
    }
}

async fn is_manager(state: &AppState, user_id: i32, company_id: i32) -> ApiResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) > 0 FROM users_positions WHERE user_id = ? AND company_id = ? AND position_id = ?",
    )
    .bind(user_id)
    .bind(company_id)
    .bind(MANAGER_POSITION)
    .fetch_one(&state.pool)
    .await?)
}

fn redirect_uri(state: &AppState, path: &str) -> String {
    format!("{}{path}", state.app_url.trim_end_matches('/'))
}

/// `QuickBooks` as company settings show it.
#[derive(Debug, Serialize, ToSchema)]
pub struct QuickbooksSettings {
    /// The `QuickBooks` feature is on for the company.
    pub feature_enabled: bool,
    /// Which app connecting uses: `company-app`, `shared-app`, or `none`
    /// (connecting is not set up).
    pub credentials_source: CredentialsSource,
    /// The company has connected an account.
    pub connected: bool,
}

/// `RingCentral` as company settings show it.
#[derive(Debug, Serialize, ToSchema)]
pub struct RingcentralSettings {
    /// Granite Manager's `RingCentral` app is configured (Connect works).
    pub connect_available: bool,
    /// For a company that pressed Connect: whether `RingCentral` still
    /// accepts the connection; null when it never connected.
    pub status: Option<ConnectionStatus>,
}

/// `GET /v1/company/settings`.
#[derive(Debug, Serialize, ToSchema)]
pub struct CompanySettings {
    /// The company these settings are of.
    pub company_id: i32,
    /// Company name, or null.
    pub name: Option<String>,
    /// Company address, or null.
    pub address: Option<String>,
    /// Logo shown in the header, or null.
    pub logo_url: Option<String>,
    /// Logo height in pixels as saved (32-192), or null.
    pub logo_height: Option<i32>,
    /// Sales tax in percent (e.g. 7.25), or null.
    pub sales_tax_percent: Option<f64>,
    /// The caller holds the Manager position in the company (may request the
    /// API bearer token).
    pub can_request_api_token: bool,
    pub quickbooks: QuickbooksSettings,
    pub ringcentral: RingcentralSettings,
}

#[derive(sqlx::FromRow)]
struct SettingsRow {
    name: Option<String>,
    address: Option<String>,
    logo_url: Option<String>,
    logo_height: Option<i32>,
    state_taxes: Option<String>,
    quickbooks_enabled: bool,
}

/// Get the company settings
///
/// The company's name, address, logo, sales tax and the state of its
/// `QuickBooks` and `RingCentral` connections (the latter checked with
/// `RingCentral`). Superusers and company managers (admins with the Manager
/// position); superusers may pass `company_id`.
#[crate::openapi::path(
    get,
    path = "/v1/company/settings",
    operation_id = "get_company_settings",
    tag = "company",
    params(TargetCompany),
    responses(
        (status = 200, description = "The settings", body = CompanySettings),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller does not manage the company", body = ErrorBody),
        (status = 404, description = "No such company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_settings(
    State(state): State<AppState>,
    UserManager(user): UserManager,
    Query(query): Query<TargetCompany>,
) -> ApiResult<Json<CompanySettings>> {
    let company_id = target(&user, &query);
    let row: SettingsRow = sqlx::query_as(
        "SELECT name, address, logo_url, logo_height, CAST(state_taxes AS CHAR) AS state_taxes,
                COALESCE(quickbooks_enabled, 1) <> 0 AS quickbooks_enabled
           FROM company WHERE id = ?",
    )
    .bind(company_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or(ApiError::NotFound)?;
    let ringcentral_status = if ringcentral_connect::is_connected(&state.pool, company_id).await? {
        Some(ringcentral_connect::check(&state.pool, &state.telephony_http, company_id).await)
    } else {
        None
    };
    Ok(Json(CompanySettings {
        company_id,
        name: row.name,
        address: row.address,
        logo_url: row.logo_url,
        logo_height: row.logo_height,
        sales_tax_percent: row
            .state_taxes
            .and_then(|tax| tax.parse::<f64>().ok())
            .map(|tax| (tax * 10_000.0).round() / 100.0),
        can_request_api_token: is_manager(&state, user.id, company_id).await?,
        quickbooks: QuickbooksSettings {
            feature_enabled: row.quickbooks_enabled,
            credentials_source: state
                .qbo
                .credentials_source(&state.pool, company_id)
                .await?,
            connected: state.qbo.is_connected(&state.pool, company_id).await?,
        },
        ringcentral: RingcentralSettings {
            connect_available: state.ringcentral_app.is_some(),
            status: ringcentral_status,
        },
    }))
}

/// Body of `PUT /v1/company/settings`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CompanySettingsInput {
    /// Company name (required).
    pub name: String,
    /// Address; blank clears it.
    #[serde(default)]
    pub address: Option<String>,
    /// A new logo uploaded to the `company-logos` folder; omit to keep the
    /// current one.
    #[serde(default)]
    pub logo_url: Option<String>,
    /// Logo height in pixels, 32 to 192.
    pub logo_height: i32,
    /// Sales tax in percent (0 to 100); null clears it.
    #[serde(default)]
    pub sales_tax_percent: Option<f64>,
}

/// Save the company settings
///
/// Replaces the name, address, logo height and sales tax, and the logo when
/// a new one is given.
#[crate::openapi::path(
    put,
    path = "/v1/company/settings",
    operation_id = "save_company_settings",
    tag = "company",
    params(TargetCompany),
    request_body = CompanySettingsInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Missing name, or a value out of range", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller does not manage the company", body = ErrorBody),
        (status = 404, description = "No such company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn save_settings(
    State(state): State<AppState>,
    UserManager(user): UserManager,
    Query(query): Query<TargetCompany>,
    JsonBody(input): JsonBody<CompanySettingsInput>,
) -> ApiResult<StatusCode> {
    let company_id = target(&user, &query);
    let name = input.name.trim();
    if name.is_empty() {
        return Err(ApiError::BadRequest("Company name is required".into()));
    }
    if !(32..=192).contains(&input.logo_height) {
        return Err(ApiError::BadRequest("logo_height must be 32 to 192".into()));
    }
    if input
        .sales_tax_percent
        .is_some_and(|tax| !tax.is_finite() || !(0.0..=100.0).contains(&tax))
    {
        return Err(ApiError::BadRequest(
            "sales_tax_percent must be 0 to 100".into(),
        ));
    }
    let existing: Option<Option<String>> =
        sqlx::query_scalar("SELECT logo_url FROM company WHERE id = ?")
            .bind(company_id)
            .fetch_optional(&state.pool)
            .await?;
    let existing = existing.ok_or(ApiError::NotFound)?;
    let logo_url = match input
        .logo_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
    {
        Some(url) => {
            uploads::attach(
                &state.storage,
                url,
                UploadFolder::CompanyLogos,
                user.company_id,
            )
            .await?;
            Some(url.to_string())
        }
        None => existing,
    };
    let address = input
        .address
        .as_deref()
        .map(str::trim)
        .filter(|address| !address.is_empty());
    sqlx::query(
        "UPDATE company SET name = ?, address = ?, logo_url = ?, logo_height = ?, state_taxes = ?
          WHERE id = ?",
    )
    .bind(name)
    .bind(address)
    .bind(logo_url)
    .bind(input.logo_height)
    .bind(
        input
            .sales_tax_percent
            .map(|tax| format!("{:.4}", (tax * 100.0).round() / 10_000.0)),
    )
    .bind(company_id)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Body of `POST /v1/company/api-token-email`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ApiTokenEmail {
    /// Where to send the token.
    pub email: String,
}

/// Email the API bearer token
///
/// Sends Granite Manager's documentation API bearer token
/// (`GRANITE_MANAGER_DOCS_BEARER_TOKEN`) to the address given. Only for
/// holders of the Manager position.
#[crate::openapi::path(
    post,
    path = "/v1/company/api-token-email",
    operation_id = "email_api_token",
    tag = "company",
    params(TargetCompany),
    request_body = ApiTokenEmail,
    responses(
        (status = 204, description = "Sent"),
        (status = 400, description = "Not an email address", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a Manager of the company", body = ErrorBody),
        (status = 502, description = "Sending failed", body = ErrorBody),
        (status = 503, description = "No token is configured", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn email_api_token(
    State(state): State<AppState>,
    UserManager(user): UserManager,
    Query(query): Query<TargetCompany>,
    JsonBody(input): JsonBody<ApiTokenEmail>,
) -> ApiResult<StatusCode> {
    let company_id = target(&user, &query);
    if !is_manager(&state, user.id, company_id).await? {
        return Err(ApiError::Denied("Manager position is required".into()));
    }
    let email = input.email.trim();
    let valid = email.split_once('@').is_some_and(|(local, domain)| {
        !local.is_empty() && domain.contains('.') && !domain.ends_with('.')
    }) && !email.chars().any(char::is_whitespace);
    if !valid {
        return Err(ApiError::BadRequest("Enter a valid email address".into()));
    }
    let token = std::env::var("GRANITE_MANAGER_DOCS_BEARER_TOKEN").unwrap_or_default();
    let mut token = token.trim();
    for prefix in ["Authorization:", "Bearer "] {
        while let Some(rest) = token
            .get(..prefix.len())
            .filter(|head| head.eq_ignore_ascii_case(prefix))
            .map(|_| &token[prefix.len()..])
        {
            token = rest.trim_start();
        }
    }
    if token.is_empty() {
        tracing::error!("GRANITE_MANAGER_DOCS_BEARER_TOKEN is not set");
        return Err(ApiError::Unavailable(
            "Bearer delivery is not configured".into(),
        ));
    }
    let body = format!(
        "Your Granite Manager API bearer token:<br><br>Bearer {token}<br><br>Keep this private. Do not forward or share it."
    );
    state
        .webhooks
        .send_email(&OutgoingEmail {
            from_name: None,
            from_address: "noreply@granite-manager.com".into(),
            to: vec![email.to_string()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "Granite Manager API bearer token".into(),
            body,
            signature: None,
            reply_to: Vec::new(),
            in_reply_to: None,
            references: Vec::new(),
            attachments: Vec::new(),
            transactional: true,
        })
        .await
        .map_err(|error| match error {
            SendFailure::Rejected(message) => ApiError::BadRequest(message),
            SendFailure::Failed(_) => ApiError::Upstream("Failed to send bearer email".into()),
        })?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- QuickBooks

/// `GET /v1/quickbooks/connection`.
#[derive(Debug, Serialize, ToSchema)]
pub struct QuickbooksConnection {
    /// The feature is on and an app is set up: Connect works.
    pub can_connect: bool,
    /// The company has connected an account (false when it cannot connect).
    pub connected: bool,
}

async fn can_connect_quickbooks(state: &AppState, company_id: i32) -> ApiResult<bool> {
    let enabled: Option<bool> =
        sqlx::query_scalar("SELECT COALESCE(quickbooks_enabled, 1) <> 0 FROM company WHERE id = ?")
            .bind(company_id)
            .fetch_optional(&state.pool)
            .await?;
    Ok(enabled.unwrap_or(false)
        && state
            .qbo
            .credentials_source(&state.pool, company_id)
            .await?
            != CredentialsSource::None)
}

/// Get the QuickBooks connection
///
/// Whether the caller's company can connect `QuickBooks` and whether it is
/// connected (for the connect page).
#[crate::openapi::path(
    get,
    path = "/v1/quickbooks/connection",
    operation_id = "get_quickbooks_connection",
    tag = "company",
    responses(
        (status = 200, description = "The connection", body = QuickbooksConnection),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn quickbooks_status(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<QuickbooksConnection>> {
    let can_connect = can_connect_quickbooks(&state, user.company_id).await?;
    let connected = can_connect && state.qbo.is_connected(&state.pool, user.company_id).await?;
    Ok(Json(QuickbooksConnection {
        can_connect,
        connected,
    }))
}

/// Where to send the browser.
#[derive(Debug, Serialize, ToSchema)]
pub struct AuthorizeLink {
    /// The provider's sign-in / consent page.
    pub url: String,
}

/// Start connecting QuickBooks
///
/// Intuit's consent page for the caller's company. Intuit returns to
/// `/redirects/quickbooks/user-auth` on the app's apex domain with `code`,
/// `state` and `realmId`; post them to `POST /v1/quickbooks/connection`
/// within 15 minutes.
#[crate::openapi::path(
    post,
    path = "/v1/quickbooks/connection/start",
    operation_id = "start_quickbooks_connection",
    tag = "company",
    responses(
        (status = 200, description = "Intuit's consent page", body = AuthorizeLink),
        (status = 400, description = "QuickBooks connect is not set up for the company", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn start_quickbooks(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<AuthorizeLink>> {
    if !can_connect_quickbooks(&state, user.company_id).await? {
        return Err(ApiError::BadRequest(
            "QuickBooks connect is not set up for your company".into(),
        ));
    }
    let state_token = oauth_state::sign(
        &state.session_secret,
        "quickbooks",
        user.company_id,
        user.id,
    );
    let url = state
        .qbo
        .authorize_url(
            &state.pool,
            user.company_id,
            &redirect_uri(&state, QBO_RETURN_PATH),
            &state_token,
        )
        .await
        .map_err(|error| match error {
            QboError::Database(error) => ApiError::Database(error),
            _ => ApiError::BadRequest("QuickBooks connect is not set up for your company".into()),
        })?;
    Ok(Json(AuthorizeLink { url }))
}

/// Body of `POST /v1/quickbooks/connection`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct QuickbooksCallback {
    /// `state` as Intuit returned it.
    pub state: String,
    /// `code` as Intuit returned it.
    pub code: String,
    /// `realmId` as Intuit returned it (the `QuickBooks` company).
    pub realm_id: String,
}

/// After connecting: where the company lives.
#[derive(Debug, Serialize, ToSchema)]
pub struct Connected {
    /// The company's subdomain, to go back to its settings there; null when
    /// it has none.
    pub company_subdomain: Option<String>,
}

/// Finish connecting QuickBooks
///
/// Trades the code Intuit returned for tokens and saves the connection. The
/// `state` must be one `POST /v1/quickbooks/connection/start` gave this user
/// for this company in the last 15 minutes (400 "connection expired"
/// otherwise).
#[crate::openapi::path(
    post,
    path = "/v1/quickbooks/connection",
    operation_id = "finish_quickbooks_connection",
    tag = "company",
    request_body = QuickbooksCallback,
    responses(
        (status = 200, description = "Connected", body = Connected),
        (status = 400, description = "Expired or foreign state, a bad realm id, or Intuit refused the code", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn finish_quickbooks(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<QuickbooksCallback>,
) -> ApiResult<Json<Connected>> {
    if !oauth_state::verify(
        &state.session_secret,
        &input.state,
        "quickbooks",
        user.company_id,
        user.id,
    ) {
        return Err(ApiError::BadRequest(
            "QuickBooks connection expired. Please try again.".into(),
        ));
    }
    let realm_id = input.realm_id.trim();
    if !qbo::valid_id(realm_id) || input.code.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "Invalid data returned from quickbooks".into(),
        ));
    }
    state
        .qbo
        .connect(
            &state.pool,
            user.company_id,
            user.id,
            input.code.trim(),
            realm_id,
            &redirect_uri(&state, QBO_RETURN_PATH),
        )
        .await
        .map_err(|error| match error {
            QboError::Database(error) => ApiError::Database(error),
            error => {
                tracing::warn!(%error, company_id = user.company_id, "QuickBooks connect failed");
                ApiError::BadRequest(
                    "Could not finish the QuickBooks connection. Try again.".into(),
                )
            }
        })?;
    Ok(Json(Connected {
        company_subdomain: company_subdomain(&state.pool, user.company_id).await?,
    }))
}

/// Disconnect QuickBooks
///
/// Forgets the company's `QuickBooks` connection.
#[crate::openapi::path(
    delete,
    path = "/v1/quickbooks/connection",
    operation_id = "disconnect_quickbooks",
    tag = "company",
    params(TargetCompany),
    responses(
        (status = 204, description = "Disconnected"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller does not manage the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn disconnect_quickbooks(
    State(state): State<AppState>,
    UserManager(user): UserManager,
    Query(query): Query<TargetCompany>,
) -> ApiResult<StatusCode> {
    qbo::disconnect(&state.pool, target(&user, &query)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Body of `POST /v1/quickbooks/disconnected`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct RealmDisconnect {
    /// `realmId` from Intuit's disconnect link; anything but 1-32 digits is
    /// ignored.
    #[serde(default)]
    pub realm_id: String,
}

/// QuickBooks was disconnected at Intuit
///
/// Intuit sends users who disconnect the app to `/quickbooks/disconnect`
/// with the `realmId`; the page posts it here and the caller's company loses
/// its connection when it holds that `QuickBooks` company. A session is
/// required: anyone could otherwise disconnect a company knowing its realm id
/// (a signed-out visitor loses nothing: the app asks to reconnect once Intuit
/// refuses the tokens). Always 204 (it never tells whether it matched).
#[crate::openapi::path(
    post,
    path = "/v1/quickbooks/disconnected",
    operation_id = "quickbooks_disconnected",
    tag = "company",
    request_body = RealmDisconnect,
    responses(
        (status = 204, description = "Done (whether or not the company matched)"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn quickbooks_disconnected(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<RealmDisconnect>,
) -> ApiResult<StatusCode> {
    qbo::disconnect_realm(&state.pool, &input.realm_id, user.company_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Body of `PUT /v1/superuser/companies/{id}/quickbooks-app`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct QuickbooksApp {
    /// The company's own Intuit app client id (10+ characters).
    pub client_id: String,
    /// Its client secret (10+ characters).
    pub client_secret: String,
}

/// Save a company's own QuickBooks app
///
/// Superusers: stores (encrypted) the client id and secret of an Intuit app
/// the company connects through instead of the shared one.
#[crate::openapi::path(
    put,
    path = "/v1/superuser/companies/{id}/quickbooks-app",
    operation_id = "save_company_quickbooks_app",
    tag = "superuser",
    params(("id" = i32, Path, description = "Company id")),
    request_body = QuickbooksApp,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Keys shorter than 10 characters", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "No such company", body = ErrorBody),
        (status = 503, description = "AES_KEY is not set", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn save_quickbooks_app(
    State(state): State<AppState>,
    UserManager(user): UserManager,
    Path(company_id): Path<i32>,
    JsonBody(input): JsonBody<QuickbooksApp>,
) -> ApiResult<StatusCode> {
    if !user.is_superuser {
        return Err(ApiError::Forbidden);
    }
    let (id, secret) = (input.client_id.trim(), input.client_secret.trim());
    if id.len() < 10 || secret.len() < 10 {
        return Err(ApiError::BadRequest(
            "client_id and client_secret need 10 or more characters".into(),
        ));
    }
    match state
        .qbo
        .save_app(&state.pool, company_id, id, secret)
        .await
    {
        Ok(true) => Ok(StatusCode::NO_CONTENT),
        Ok(false) => Err(ApiError::NotFound),
        Err(QboError::Database(error)) => Err(ApiError::Database(error)),
        Err(_) => Err(ApiError::Unavailable("AES_KEY is not set".into())),
    }
}

// ---- RingCentral

/// Start connecting RingCentral
///
/// `RingCentral`'s sign-in page for the caller's company. `RingCentral`
/// returns to `/redirects/ringcentral/user-auth` on the app's apex domain
/// with `code` and `state` (or `error`); post them to
/// `POST /v1/ringcentral/connection` within 15 minutes.
#[crate::openapi::path(
    post,
    path = "/v1/ringcentral/connection/start",
    operation_id = "start_ringcentral_connection",
    tag = "company",
    responses(
        (status = 200, description = "RingCentral's sign-in page", body = AuthorizeLink),
        (status = 400, description = "RingCentral connect is not set up", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn start_ringcentral(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<AuthorizeLink>> {
    let app = state.ringcentral_app.as_ref().ok_or_else(|| {
        ApiError::BadRequest("RingCentral connect is not set up yet. Ask a superuser.".into())
    })?;
    let state_token = oauth_state::sign(
        &state.session_secret,
        "ringcentral",
        user.company_id,
        user.id,
    );
    Ok(Json(AuthorizeLink {
        url: app.authorize_url(&redirect_uri(&state, RC_RETURN_PATH), &state_token),
    }))
}

/// Body of `POST /v1/ringcentral/connection`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct RingcentralCallback {
    /// `state` as `RingCentral` returned it.
    pub state: String,
    /// `code` as `RingCentral` returned it; absent when the user cancelled.
    #[serde(default)]
    pub code: Option<String>,
    /// `error` as `RingCentral` returned it (the user cancelled).
    #[serde(default)]
    pub error: Option<String>,
}

/// Finish connecting RingCentral
///
/// Trades the code for tokens, saves the connection and subscribes to the
/// company's texts (a failed subscription does not undo the connect).
#[crate::openapi::path(
    post,
    path = "/v1/ringcentral/connection",
    operation_id = "finish_ringcentral_connection",
    tag = "company",
    request_body = RingcentralCallback,
    responses(
        (status = 200, description = "Connected", body = Connected),
        (status = 400, description = "Cancelled, expired or foreign state, or RingCentral refused the code", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn finish_ringcentral(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<RingcentralCallback>,
) -> ApiResult<Json<Connected>> {
    if input
        .error
        .as_deref()
        .is_some_and(|error| !error.is_empty())
    {
        return Err(ApiError::BadRequest(
            "RingCentral connection was cancelled.".into(),
        ));
    }
    if !oauth_state::verify(
        &state.session_secret,
        &input.state,
        "ringcentral",
        user.company_id,
        user.id,
    ) {
        return Err(ApiError::BadRequest(
            "RingCentral connection expired. Please try again.".into(),
        ));
    }
    let Some(code) = input
        .code
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
    else {
        return Err(ApiError::BadRequest(
            "Invalid data returned from RingCentral.".into(),
        ));
    };
    let app = state.ringcentral_app.as_ref().ok_or_else(|| {
        ApiError::BadRequest("RingCentral connect is not set up yet. Ask a superuser.".into())
    })?;
    if let Err(error) = app
        .connect(
            &state.pool,
            &state.telephony_http,
            user.company_id,
            user.id,
            code,
            &redirect_uri(&state, RC_RETURN_PATH),
        )
        .await
    {
        if let ringcentral_connect::ConnectError::Database(error) = error {
            return Err(ApiError::Database(error));
        }
        tracing::warn!(%error, company_id = user.company_id, "RingCentral connect failed");
        return Err(ApiError::BadRequest(
            "Could not finish the RingCentral connection. Try again.".into(),
        ));
    }
    // Inbound texts only reach the CRM through this subscription.
    if let Err(error) = common::telephony::ringcentral::ensure_sms_subscription(
        &state.pool,
        &state.telephony_http,
        user.company_id,
    )
    .await
    {
        tracing::warn!(
            ?error,
            company_id = user.company_id,
            "RingCentral SMS subscription failed"
        );
    }
    Ok(Json(Connected {
        company_subdomain: company_subdomain(&state.pool, user.company_id).await?,
    }))
}

/// Disconnect RingCentral
///
/// Stops `RingCentral` pushing the company's texts and forgets the
/// connection.
#[crate::openapi::path(
    delete,
    path = "/v1/ringcentral/connection",
    operation_id = "disconnect_ringcentral",
    tag = "company",
    params(TargetCompany),
    responses(
        (status = 204, description = "Disconnected"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller does not manage the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn disconnect_ringcentral(
    State(state): State<AppState>,
    UserManager(user): UserManager,
    Query(query): Query<TargetCompany>,
) -> ApiResult<StatusCode> {
    ringcentral_connect::disconnect(&state.pool, &state.telephony_http, target(&user, &query))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
