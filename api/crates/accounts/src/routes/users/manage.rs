//! Managing a company's users (the admin Users page): list, add, edit,
//! delete, with their positions.
//!
//! - `GET    /v1/admin/users`              → the users and the page's settings
//! - `POST   /v1/admin/users`              → add a user by email, invite them
//! - `GET    /v1/admin/users/{id}`         → one user for the edit form
//! - `PUT    /v1/admin/users/{id}`         → save them
//! - `DELETE /v1/admin/users/{id}`         → delete them (soft)
//! - `POST   /v1/admin/users/{id}/invite`  → email them a new invite link
//!
//! Superusers manage every company; a Manager admin their own. Superusers
//! and Super Admins can only be changed by a superuser. A new user sets their
//! own name, phone and password from the invite link (`auth.rs`). Replaces
//! the Remix `admin.users*` loaders and actions.

use crate::auth::UserManager;
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::{JsonBody, MultiQuery};
use crate::openapi::{IntoParams, ToSchema};
use crate::routes::auth::{SentInvite, send_invite};
use crate::routes::company::queries::company_features;
use crate::routes::sms::provider::phone10;
use crate::routes::users::texting_history;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use common::telephony::ringcentral::{self, Body};
use lambda_http::tracing;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::MySqlPool;
use std::collections::HashSet;

pub const SALES_REP: i32 = 1;
pub const SHOP_WORKER: i32 = 4;
pub const EXTERNAL_MARKETING: i32 = 7;
pub const CHECK_IN: i32 = 8;
pub const SUPER_ADMIN: i32 = 9;
/// Every position id there is (`POSITIONS`).
const POSITIONS: [i32; 11] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];

/// Query of `GET /v1/admin/users`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct UsersQuery {
    /// Superusers: one company's users (default every company's).
    pub company_id: Option<i32>,
}

/// A user in the list.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct ManagedUser {
    /// User id.
    pub id: i32,
    /// Name.
    pub name: Option<String>,
    /// Email.
    pub email: Option<String>,
    /// Phone.
    pub phone_number: Option<String>,
    /// `CloudTalk` agent id.
    pub cloudtalk_agent_id: Option<String>,
    /// `CloudTalk` outbound number.
    pub cloudtalk_phone_number: Option<String>,
    /// `RingCentral` extension id.
    pub ringcentral_extension_id: Option<String>,
    /// `RingCentral` outbound number.
    pub ringcentral_phone_number: Option<String>,
    /// A superuser or Super Admin: only superusers may change them.
    pub is_protected: bool,
    /// Has not set up their account from the invite yet (no password).
    pub invite_pending: bool,
}

/// A company to pick.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct CompanyName {
    /// Company id.
    pub id: i32,
    /// Name.
    pub name: Option<String>,
}

/// How many users a company has and may have.
#[derive(Debug, Serialize, ToSchema)]
pub struct UserLimit {
    /// Company id.
    pub company_id: i32,
    /// Users counting toward the limit (check-in-only accounts do not).
    pub current_count: i64,
    /// The limit; null for none.
    pub max_users: Option<i32>,
}

/// The Users page.
#[derive(Debug, Serialize, ToSchema)]
pub struct UsersPage {
    /// The users, live only.
    pub users: Vec<ManagedUser>,
    /// Companies the caller may pick.
    pub companies: Vec<CompanyName>,
    /// The company listed; null for every company (superusers).
    pub company_id: Option<i32>,
    /// The caller is a superuser.
    pub is_super_user: bool,
    /// Each pickable company's user count and limit.
    pub user_limits: Vec<UserLimit>,
    /// The listed company has `CloudTalk`.
    pub has_cloudtalk_api: bool,
    /// The listed company has `RingCentral`.
    pub has_ringcentral_api: bool,
}

const PROTECTED: &str = "(u.is_superuser = 1 OR EXISTS (SELECT 1 FROM users_positions up \
     WHERE up.user_id = u.id AND up.position_id = 9))";

/// No password yet: invited, not set up.
const PENDING: &str = "(u.password IS NULL OR u.password = '')";

/// Users counting toward `max_users` (`COUNTS_TOWARD_COMPANY_USER_LIMIT_SQL`).
async fn user_limit(pool: &MySqlPool, company_id: i32) -> Result<UserLimit, sqlx::Error> {
    let current_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM users u
          WHERE u.company_id = ? AND u.is_deleted = 0
            AND (EXISTS (SELECT 1 FROM users_positions up WHERE up.user_id = u.id
                          AND up.company_id = u.company_id AND up.position_id <> 8)
                 OR NOT EXISTS (SELECT 1 FROM users_positions up WHERE up.user_id = u.id
                          AND up.company_id = u.company_id))",
    )
    .bind(company_id)
    .fetch_one(pool)
    .await?;
    let max_users: Option<Option<i32>> =
        sqlx::query_scalar("SELECT CAST(max_users AS SIGNED) FROM company WHERE id = ?")
            .bind(company_id)
            .fetch_optional(pool)
            .await
            .or_else(|_| Ok::<_, sqlx::Error>(None))?;
    Ok(UserLimit {
        company_id,
        current_count,
        max_users: max_users.flatten(),
    })
}

async fn companies(pool: &MySqlPool, user: &SessionUser) -> Result<Vec<CompanyName>, sqlx::Error> {
    if user.is_superuser {
        sqlx::query_as("SELECT id, name FROM company WHERE deleted_at IS NULL ORDER BY id")
            .fetch_all(pool)
            .await
    } else {
        sqlx::query_as("SELECT id, name FROM company WHERE deleted_at IS NULL AND id = ?")
            .bind(user.company_id)
            .fetch_all(pool)
            .await
    }
}

/// List users
///
/// The live users of the caller's company (superusers: of `company_id`, or
/// of every company), which of them only a superuser may change, the
/// companies the caller may pick with their user counts and limits, and
/// whether the listed company has `CloudTalk` or `RingCentral`.
#[crate::openapi::path(
    get,
    path = "/v1/admin/users",
    operation_id = "list_managed_users",
    tag = "users",
    params(UsersQuery),
    responses(
        (status = 200, description = "The page", body = UsersPage),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller may not manage users", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    UserManager(user): UserManager,
    MultiQuery(query): MultiQuery<UsersQuery>,
) -> ApiResult<Json<UsersPage>> {
    let pool = &state.pool;
    let company_id = if user.is_superuser {
        query.company_id
    } else {
        Some(user.company_id)
    };
    let columns = format!(
        "u.id, u.name, u.email, u.phone_number, u.cloudtalk_agent_id, u.cloudtalk_phone_number,
         u.ringcentral_extension_id, u.ringcentral_phone_number, {PROTECTED} AS is_protected,
         {PENDING} AS invite_pending"
    );
    let users: Vec<ManagedUser> = match company_id {
        Some(company) => {
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT {columns} FROM users u WHERE u.is_deleted = 0 AND u.company_id = ? ORDER BY u.id"
            )))
            .bind(company)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT {columns} FROM users u WHERE u.is_deleted = 0 ORDER BY u.id"
            )))
            .fetch_all(pool)
            .await?
        }
    };
    let companies = companies(pool, &user).await?;
    let mut user_limits = Vec::new();
    for company in &companies {
        user_limits.push(user_limit(pool, company.id).await?);
    }
    let (has_cloudtalk_api, has_ringcentral_api) = match company_id {
        Some(company) => {
            let features = company_features(pool, company).await?;
            (features.cloudtalk, features.ringcentral)
        }
        None => (false, false),
    };
    Ok(Json(UsersPage {
        users,
        companies,
        company_id,
        is_super_user: user.is_superuser,
        user_limits,
        has_cloudtalk_api,
        has_ringcentral_api,
    }))
}

/// `positionConflictError`.
fn position_conflict(positions: &[i32]) -> Option<&'static str> {
    (positions.contains(&SALES_REP) && positions.contains(&SHOP_WORKER))
        .then_some("A user can't be both Sales Representative and Shop Worker.")
}

/// `computeIsEmployeeFlag`: everyone but external marketing and shop
/// workers alone.
fn is_employee(positions: &[i32]) -> bool {
    positions.is_empty()
        || positions
            .iter()
            .any(|id| *id != EXTERNAL_MARKETING && *id != SHOP_WORKER)
}

/// Only superusers grant Super Admin and check-in.
fn grantable(user: &SessionUser, positions: &[i32]) -> Vec<i32> {
    let mut seen = HashSet::new();
    positions
        .iter()
        .copied()
        .filter(|id| POSITIONS.contains(id))
        .filter(|id| user.is_superuser || (*id != SUPER_ADMIN && *id != CHECK_IN))
        .filter(|id| seen.insert(*id))
        .collect()
}

fn checked_phone(phone: Option<&str>) -> ApiResult<Option<String>> {
    match phone.map(str::trim).filter(|p| !p.is_empty()) {
        None => Ok(None),
        Some(phone) if phone.chars().count() >= 10 => Ok(Some(phone.to_string())),
        Some(_) => Err(ApiError::BadRequest(
            "Phone number must have at least 10 characters".into(),
        )),
    }
}

fn checked_email(email: &str) -> ApiResult<String> {
    let email = email.trim().to_lowercase();
    if crate::routes::me::account::valid_email(&email) {
        Ok(email)
    } else {
        Err(ApiError::BadRequest("Enter a valid email".into()))
    }
}

/// Body of `POST /v1/admin/users`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewUser {
    /// Email: the invite goes there, and they sign in with it.
    pub email: String,
    /// Superusers: the company (others add to their own).
    #[serde(default)]
    pub company_id: Option<i32>,
    /// Position ids.
    #[serde(default)]
    pub positions: Vec<i32>,
    /// Admin access.
    #[serde(default)]
    pub is_admin: bool,
}

/// A new user and their invite.
#[derive(Debug, Serialize, ToSchema)]
pub struct CreatedUser {
    /// The new user.
    pub id: i32,
    /// The invite link, to share another way if the email does not arrive.
    pub invite_url: String,
    /// The invite was emailed to them.
    pub email_sent: bool,
}

/// Add a user
///
/// Creates a user with their email and positions in the caller's company
/// (superusers: in `company_id`) and emails them an invite link (7 days) to
/// set their own name, phone and password; they cannot sign in before. The
/// email must be unused, even by a deleted user. When the email fails the
/// user is still added: share the returned link instead.
#[crate::openapi::path(
    post,
    path = "/v1/admin/users",
    operation_id = "create_managed_user",
    tag = "users",
    request_body = NewUser,
    responses(
        (status = 201, description = "Added and invited", body = CreatedUser),
        (status = 400, description = "Invalid email, or positions that can't go together", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller may not manage users", body = ErrorBody),
        (status = 409, description = "The email is taken", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create(
    State(state): State<AppState>,
    UserManager(user): UserManager,
    JsonBody(input): JsonBody<NewUser>,
) -> ApiResult<(StatusCode, Json<CreatedUser>)> {
    let email = checked_email(&input.email)?;
    let company_id = if user.is_superuser {
        input
            .company_id
            .ok_or_else(|| ApiError::BadRequest("Select a company".into()))?
    } else {
        user.company_id
    };
    let positions = grantable(&user, &input.positions);
    if let Some(conflict) = position_conflict(&positions) {
        return Err(ApiError::BadRequest(conflict.into()));
    }
    let existing: Option<bool> = sqlx::query_scalar(
        "SELECT is_deleted <> 0 FROM users WHERE LOWER(TRIM(email)) = ? LIMIT 1",
    )
    .bind(&email)
    .fetch_optional(&state.pool)
    .await?;
    if let Some(deleted) = existing {
        return Err(ApiError::Conflict(if deleted {
            "This email belongs to a deleted user. Use a different email.".into()
        } else {
            "A user with this email already exists".into()
        }));
    }
    let mut tx = state.pool.begin().await?;
    // No password until they set up the account; the name is theirs to fill.
    let inserted = sqlx::query(
        "INSERT INTO users (name, email, company_id, is_employee, is_admin)
         VALUES ('', ?, ?, ?, ?)",
    )
    .bind(&email)
    .bind(company_id)
    .bind(is_employee(&positions))
    .bind(input.is_admin)
    .execute(&mut *tx)
    .await;
    let id = match inserted {
        Ok(result) => result.last_insert_id(),
        Err(sqlx::Error::Database(err)) if err.is_unique_violation() => {
            return Err(ApiError::Conflict(
                "A user with this email already exists".into(),
            ));
        }
        Err(err) => return Err(err.into()),
    };
    for position in &positions {
        sqlx::query(
            "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (?, ?, ?)",
        )
        .bind(id)
        .bind(position)
        .bind(company_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    let id = i32::try_from(id).unwrap_or(i32::MAX);
    let invite = send_invite(&state, id).await?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedUser {
            id,
            invite_url: invite.invite_url,
            email_sent: invite.email_sent,
        }),
    ))
}

#[derive(Debug, sqlx::FromRow)]
struct TargetRow {
    company_id: i32,
    is_protected: bool,
}

async fn invite_pending(pool: &MySqlPool, id: i32) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT {PENDING} FROM users u WHERE u.id = ?"
    )))
    .bind(id)
    .fetch_one(pool)
    .await
}

/// The user the caller may change: live, of their company (superusers: any),
/// and not protected unless the caller is a superuser.
async fn target(pool: &MySqlPool, user: &SessionUser, id: i32) -> ApiResult<i32> {
    let row: Option<TargetRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT u.company_id, {PROTECTED} AS is_protected FROM users u WHERE u.id = ? AND u.is_deleted = 0"
    )))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    let row = row.ok_or(ApiError::NotFound)?;
    if !user.is_superuser && row.company_id != user.company_id {
        return Err(ApiError::NotFound);
    }
    if row.is_protected && !user.is_superuser {
        return Err(ApiError::Forbidden);
    }
    Ok(row.company_id)
}

/// The edit form's user.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct EditedUser {
    /// User id.
    pub id: i32,
    /// Name.
    pub name: Option<String>,
    /// Email.
    pub email: Option<String>,
    /// Phone.
    pub phone_number: Option<String>,
    /// Their company.
    pub company_id: i32,
    /// Admin access.
    pub is_admin: bool,
    /// A superuser.
    pub is_superuser: bool,
    /// An employee.
    pub is_employee: bool,
    /// `CloudTalk` agent id.
    pub cloudtalk_agent_id: Option<String>,
    /// `CloudTalk` outbound number.
    pub cloudtalk_phone_number: Option<String>,
    /// `RingCentral` extension id.
    pub ringcentral_extension_id: Option<String>,
    /// `RingCentral` outbound number.
    pub ringcentral_phone_number: Option<String>,
    /// Has not set up their account from the invite yet (no password).
    pub invite_pending: bool,
}

/// The edit form.
#[derive(Debug, Serialize, ToSchema)]
pub struct UserForm {
    /// The user.
    pub user: EditedUser,
    /// Their position ids.
    pub positions: Vec<i32>,
    /// Companies they market for (external marketing).
    pub marketing_company_ids: Vec<i32>,
    /// Companies they are Super Admin of.
    pub superadmin_company_ids: Vec<i32>,
    /// Companies the caller may pick.
    pub companies: Vec<CompanyName>,
    /// The caller is a superuser.
    pub logged_in_is_superuser: bool,
    /// Their company has `CloudTalk`.
    pub has_cloudtalk_api: bool,
    /// Their company has `RingCentral`.
    pub has_ringcentral_api: bool,
}

async fn position_companies(
    pool: &MySqlPool,
    user_id: i32,
    position: i32,
) -> Result<Vec<i32>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT company_id FROM users_positions WHERE user_id = ? AND position_id = ? ORDER BY id",
    )
    .bind(user_id)
    .bind(position)
    .fetch_all(pool)
    .await
}

/// Get a user to edit
///
/// The user with their positions, marketing and Super Admin companies, the
/// companies the caller may pick (non-superusers: the user's own and the
/// ones a superuser granted marketing for) and their company's telephony.
#[crate::openapi::path(
    get,
    path = "/v1/admin/users/{id}",
    operation_id = "get_managed_user",
    tag = "users",
    params(("id" = i32, Path, description = "User id")),
    responses(
        (status = 200, description = "The form", body = UserForm),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller may not manage users, or only a superuser may change this one", body = ErrorBody),
        (status = 404, description = "No such live user of the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get(
    State(state): State<AppState>,
    UserManager(user): UserManager,
    Path(id): Path<i32>,
) -> ApiResult<Json<UserForm>> {
    let pool = &state.pool;
    target(pool, &user, id).await?;
    let edited: EditedUser = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT u.id, u.name, u.email, u.phone_number, u.company_id,
                COALESCE(u.is_admin, 0) <> 0 AS is_admin, COALESCE(u.is_superuser, 0) <> 0 AS is_superuser,
                COALESCE(u.is_employee, 0) <> 0 AS is_employee, u.cloudtalk_agent_id,
                u.cloudtalk_phone_number, u.ringcentral_extension_id, u.ringcentral_phone_number,
                {PENDING} AS invite_pending
           FROM users u WHERE u.id = ?"
    )))
    .bind(id)
    .fetch_one(pool)
    .await?;
    let positions: Vec<i32> =
        sqlx::query_scalar("SELECT DISTINCT position_id FROM users_positions WHERE user_id = ?")
            .bind(id)
            .fetch_all(pool)
            .await?;
    let marketing_company_ids = position_companies(pool, id, EXTERNAL_MARKETING).await?;
    let superadmin_company_ids = position_companies(pool, id, SUPER_ADMIN).await?;
    let companies: Vec<CompanyName> = if user.is_superuser {
        sqlx::query_as("SELECT id, name FROM company WHERE deleted_at IS NULL ORDER BY id")
            .fetch_all(pool)
            .await?
    } else {
        let mut ids = vec![edited.company_id];
        ids.extend(&marketing_company_ids);
        let marks = vec!["?"; ids.len()].join(",");
        let mut sql = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT id, name FROM company WHERE deleted_at IS NULL AND id IN ({marks}) ORDER BY id"
        )));
        for id in &ids {
            sql = sql.bind(id);
        }
        sql.fetch_all(pool).await?
    };
    let features = company_features(pool, edited.company_id).await?;
    Ok(Json(UserForm {
        user: edited,
        positions,
        marketing_company_ids,
        superadmin_company_ids,
        companies,
        logged_in_is_superuser: user.is_superuser,
        has_cloudtalk_api: features.cloudtalk,
        has_ringcentral_api: features.ringcentral,
    }))
}

/// Body of `PUT /v1/admin/users/{id}`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UserChanges {
    /// Name (may stay empty while their invite is pending).
    pub name: String,
    /// Email, or empty for none.
    #[serde(default)]
    pub email: Option<String>,
    /// Phone (10+ characters) or empty.
    #[serde(default)]
    pub phone_number: Option<String>,
    /// Superusers: move them to this company.
    pub company_id: i32,
    /// Position ids.
    #[serde(default)]
    pub positions: Vec<i32>,
    /// External marketing: the companies they market for.
    #[serde(default)]
    pub marketing_company_ids: Vec<i32>,
    /// Superusers: the companies they are Super Admin of.
    #[serde(default)]
    pub superadmin_company_ids: Vec<i32>,
    /// Admin access (always on for Super Admins).
    #[serde(default)]
    pub is_admin: bool,
    /// `CloudTalk` agent id (digits) or empty.
    #[serde(default)]
    pub cloudtalk_agent_id: Option<String>,
    /// `CloudTalk` number (10+ digits) or empty.
    #[serde(default)]
    pub cloudtalk_phone_number: Option<String>,
    /// `RingCentral` number (10+ digits) or empty; the extension is looked up
    /// from it.
    #[serde(default)]
    pub ringcentral_phone_number: Option<String>,
}

fn digits_only(value: Option<&str>, label: &str, min: usize) -> ApiResult<Option<String>> {
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
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
    Ok(Some(value.to_string()))
}

/// A user's `RingCentral` line, resolved from their number.
pub struct ExtensionLink {
    /// The extension to store.
    pub extension_id: Option<String>,
    /// `RingCentral` has an extension holding the number (otherwise the
    /// stored extension was kept).
    pub matched: bool,
}

/// The `RingCentral` extension holding `phone` (`resolveRingCentralExtension`):
/// a failed or empty lookup keeps the stored one, no number clears it.
pub async fn resolve_ringcentral_extension(
    state: &AppState,
    company_id: i32,
    user_id: i32,
    phone: Option<&str>,
) -> Result<ExtensionLink, sqlx::Error> {
    let kept = |extension_id: Option<String>| ExtensionLink {
        extension_id,
        matched: false,
    };
    let stored: Option<Option<String>> = sqlx::query_scalar(
        "SELECT ringcentral_extension_id FROM users WHERE id = ? AND company_id = ? AND is_deleted = 0",
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(&state.pool)
    .await?;
    let current = stored.flatten();
    let Some(phone) = phone else {
        return Ok(kept(None));
    };
    if !company_features(&state.pool, company_id).await?.ringcentral {
        return Ok(kept(current));
    }
    let pool = &state.pool;
    let client = &state.telephony_http;
    let list = ringcentral::request(
        pool,
        client,
        company_id,
        Method::GET,
        "account/~/extension?type=User&status=Enabled&perPage=250",
        &Body::Empty,
        None,
    )
    .await;
    let Ok(list) = list else {
        return Ok(kept(current));
    };
    for record in list["records"].as_array().into_iter().flatten() {
        let Some(id) = record["id"]
            .as_i64()
            .map(|id| id.to_string())
            .or_else(|| record["id"].as_str().map(str::to_string))
        else {
            continue;
        };
        let numbers = ringcentral::request(
            pool,
            client,
            company_id,
            Method::GET,
            &format!("account/~/extension/{id}/phone-number?perPage=100"),
            &Body::Empty,
            None,
        )
        .await
        .unwrap_or(Value::Null);
        let matches = numbers["records"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|number| number["phoneNumber"].as_str())
            .any(|number| phone10(number) == phone);
        if matches {
            return Ok(ExtensionLink {
                extension_id: Some(id),
                matched: true,
            });
        }
    }
    tracing::info!(user_id, "no RingCentral extension holds the number");
    Ok(kept(current))
}

#[derive(Debug, Clone, Copy, sqlx::FromRow)]
struct PositionRow {
    id: i64,
    position_id: i32,
    company_id: i32,
}

/// Save a user
///
/// Saves the user's details and positions. Positions that stay keep their
/// rows (Telegram lead buttons point at them). Non-superusers keep the
/// user's company, Super Admin and check-in positions and Super Admin
/// companies as they are, and may only grant marketing for the user's own
/// company (or remove companies a superuser granted). Super Admins are
/// always admins. The `RingCentral` extension follows the number. A user
/// who is no longer an employee or admin of their company (or moved to
/// another one) is signed out of Granite Phone.
#[crate::openapi::path(
    put,
    path = "/v1/admin/users/{id}",
    operation_id = "update_managed_user",
    tag = "users",
    params(("id" = i32, Path, description = "User id")),
    request_body = UserChanges,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Missing or invalid field, or positions that can't go together", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller may not manage users, or only a superuser may change this one", body = ErrorBody),
        (status = 404, description = "No such live user of the caller's company", body = ErrorBody),
        (status = 409, description = "The email is taken", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
#[allow(clippy::too_many_lines)]
pub async fn update(
    State(state): State<AppState>,
    UserManager(user): UserManager,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<UserChanges>,
) -> ApiResult<StatusCode> {
    let pool = &state.pool;
    let existing_company = target(pool, &user, id).await?;
    let name = input.name.trim();
    if name.is_empty() && !invite_pending(pool, id).await? {
        return Err(ApiError::BadRequest("Name is required".into()));
    }
    let phone = checked_phone(input.phone_number.as_deref())?;
    let email = match input
        .email
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
    {
        Some(email) => Some(checked_email(email)?),
        None => None,
    };
    let cloudtalk_agent =
        digits_only(input.cloudtalk_agent_id.as_deref(), "CloudTalk agent ID", 1)?;
    let cloudtalk_phone = digits_only(
        input.cloudtalk_phone_number.as_deref(),
        "CloudTalk phone",
        10,
    )?
    .map(|p| phone10(&p));
    let ringcentral_phone = digits_only(
        input.ringcentral_phone_number.as_deref(),
        "RingCentral phone",
        10,
    )?
    .map(|p| phone10(&p));

    let existing_rows: Vec<PositionRow> = sqlx::query_as(
        "SELECT CAST(id AS SIGNED) AS id, position_id, company_id FROM users_positions WHERE user_id = ? ORDER BY id",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    let existing_for = |position: i32| -> Vec<i32> {
        existing_rows
            .iter()
            .filter(|row| row.position_id == position)
            .map(|row| row.company_id)
            .collect()
    };
    let mut positions = grantable(&user, &input.positions);
    let mut company_id = input.company_id;
    let mut marketing = input.marketing_company_ids.clone();
    let mut superadmin = input.superadmin_company_ids.clone();
    if !user.is_superuser {
        company_id = existing_company;
        superadmin = existing_for(SUPER_ADMIN);
        let mut allowed: HashSet<i32> = existing_for(EXTERNAL_MARKETING).into_iter().collect();
        allowed.insert(existing_company);
        marketing.retain(|company| allowed.contains(company));
        for row in &existing_rows {
            if (row.position_id == SUPER_ADMIN || row.position_id == CHECK_IN)
                && !positions.contains(&row.position_id)
            {
                positions.push(row.position_id);
            }
        }
    }
    let is_admin = input.is_admin || positions.contains(&SUPER_ADMIN);
    if let Some(conflict) = position_conflict(&positions) {
        return Err(ApiError::BadRequest(conflict.into()));
    }
    if let Some(email) = &email {
        let taken: Option<i32> = sqlx::query_scalar(
            "SELECT id FROM users WHERE LOWER(TRIM(email)) = ? AND id <> ? LIMIT 1",
        )
        .bind(email)
        .bind(id)
        .fetch_optional(pool)
        .await?;
        if taken.is_some() {
            return Err(ApiError::Conflict(
                "A user with this email already exists".into(),
            ));
        }
    }
    let before: (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT cloudtalk_agent_id, cloudtalk_phone_number, ringcentral_extension_id,
                ringcentral_phone_number
           FROM users WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await?;
    let link =
        resolve_ringcentral_extension(&state, existing_company, id, ringcentral_phone.as_deref())
            .await?;
    let extension = link.extension_id;

    let dedupe = |list: Vec<i32>| {
        let mut seen = HashSet::new();
        list.into_iter()
            .filter(|c| seen.insert(*c))
            .collect::<Vec<_>>()
    };
    let marketing = dedupe(marketing);
    let superadmin = dedupe(superadmin);
    let next: Vec<(i32, i32)> = positions
        .iter()
        .flat_map(|position| {
            let companies = match *position {
                EXTERNAL_MARKETING if !marketing.is_empty() => marketing.clone(),
                SUPER_ADMIN if !superadmin.is_empty() => superadmin.clone(),
                _ => vec![company_id],
            };
            companies
                .into_iter()
                .map(move |company| (*position, company))
        })
        .collect();
    let wanted: HashSet<(i32, i32)> = next.iter().copied().collect();
    let mut kept = HashSet::new();
    let mut removed = Vec::new();
    for row in &existing_rows {
        let key = (row.position_id, row.company_id);
        if wanted.contains(&key) && kept.insert(key) {
            continue;
        }
        removed.push(row.id);
    }
    let added: Vec<(i32, i32)> = next.into_iter().filter(|key| !kept.contains(key)).collect();

    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE users SET name = ?, email = ?, phone_number = ?, company_id = ?, is_admin = ?,
                is_employee = ?, cloudtalk_agent_id = ?, cloudtalk_phone_number = ?,
                ringcentral_extension_id = ?, ringcentral_phone_number = ?
          WHERE id = ?",
    )
    .bind(name)
    .bind(&email)
    .bind(&phone)
    .bind(company_id)
    .bind(is_admin)
    .bind(is_employee(&positions))
    .bind(&cloudtalk_agent)
    .bind(&cloudtalk_phone)
    .bind(&extension)
    .bind(&ringcentral_phone)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    for row_id in removed {
        sqlx::query("DELETE FROM users_positions WHERE id = ? AND user_id = ?")
            .bind(row_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    for (position, company) in added {
        sqlx::query(
            "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (?, ?, ?)",
        )
        .bind(id)
        .bind(position)
        .bind(company)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    let line10 = |phone: Option<String>| phone.map(|p| phone10(&p)).filter(|p| p.len() == 10);
    let (agent_before, phone_before, extension_before, ringcentral_before) = before;
    texting_history::follow(
        &state,
        existing_company,
        id,
        &texting_history::Line {
            cloudtalk_agent: agent_before,
            cloudtalk_phone: line10(phone_before),
            ringcentral_extension: extension_before,
            ringcentral_phone: line10(ringcentral_before),
        },
        &texting_history::Line {
            cloudtalk_agent,
            cloudtalk_phone,
            ringcentral_extension: extension,
            ringcentral_phone,
        },
        link.matched,
    )
    .await;
    crate::phone::revoke::revoke_if_lost_access(&state, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a user
///
/// Soft-deletes a live user of the caller's company (superusers: any) and
/// signs out their Granite Phone apps.
#[crate::openapi::path(
    delete,
    path = "/v1/admin/users/{id}",
    operation_id = "delete_managed_user",
    tag = "users",
    params(("id" = i32, Path, description = "User id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller may not manage users, or only a superuser may delete this one", body = ErrorBody),
        (status = 404, description = "No such live user of the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete(
    State(state): State<AppState>,
    UserManager(user): UserManager,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    target(&state.pool, &user, id).await?;
    sqlx::query("UPDATE users SET is_deleted = 1 WHERE id = ? AND is_deleted = 0")
        .bind(id)
        .execute(&state.pool)
        .await?;
    crate::phone::revoke::revoke_user_phones(
        &state,
        id,
        crate::phone::revoke::RevokeReason::UserRemoved,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Resend a user's invite
///
/// Emails a user who has not set up their account a new invite link (7
/// days); older links stop working. When the email fails the link is still
/// answered, to share another way.
#[crate::openapi::path(
    post,
    path = "/v1/admin/users/{id}/invite",
    operation_id = "resend_user_invite",
    tag = "users",
    params(("id" = i32, Path, description = "User id")),
    responses(
        (status = 200, description = "A new link was made", body = SentInvite),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller may not manage users, or only a superuser may change this one", body = ErrorBody),
        (status = 404, description = "No such live user of the caller's company", body = ErrorBody),
        (status = 409, description = "They already set up their account", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn invite(
    State(state): State<AppState>,
    UserManager(user): UserManager,
    Path(id): Path<i32>,
) -> ApiResult<Json<SentInvite>> {
    target(&state.pool, &user, id).await?;
    if !invite_pending(&state.pool, id).await? {
        return Err(ApiError::Conflict(
            "This user already set up their account".into(),
        ));
    }
    Ok(Json(send_invite(&state, id).await?))
}

#[cfg(test)]
mod tests {
    use super::{is_employee, position_conflict};

    #[test]
    fn position_rules() {
        assert!(position_conflict(&[1, 4]).is_some());
        assert!(position_conflict(&[1, 2]).is_none());
        assert!(is_employee(&[]));
        assert!(!is_employee(&[4, 7]));
        assert!(is_employee(&[4, 6]));
    }
}
