//! The company admin's Granite Phone page: the company's numbers, who they
//! ring and who may call from them, the main line's business hours and
//! holidays, call recording, and the company's signed-in phones.
//! Session cookie only; company admins, super admins and superusers
//! (`AdminUser`, which covers every user manager). Changes are written to
//! `phone_audit_log`.

use super::superuser::{checked_holidays, checked_hours, http_url};
use super::{audit, telnyx_failed};
use crate::auth::AdminUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::phone::availability::WorkingHours;
use crate::phone::company::{self, RecordingMode};
use crate::phone::lines::NumberKind;
use crate::phone::numbers::{is_nanp, last10, to_e164};
use crate::phone::revoke::{RevokeReason, revoke_device_session};
use crate::phone::routing::{NoAnswerAction, PHONE_USER};
use crate::routes::device_sessions::DevicePlatform;
use crate::serde_helpers::{js_date, js_date_required};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{FromRow, MySqlPool};
use std::collections::{HashMap, HashSet};
use utoipa::ToSchema;

/// Longest number label.
const LABEL_CHARS: usize = 100;
/// Longest voicemail greeting.
const GREETING_CHARS: usize = 500;
/// Most people one number may list.
const MAX_ASSIGNMENTS: usize = 500;

/// Who a number rings and who may call from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NumberAssignment {
    /// A user of the company.
    pub user_id: i32,
    /// Calls to the number ring this user's phones.
    pub rings: bool,
    /// The user may call from the number (it is one of their caller IDs).
    pub can_call_from: bool,
}

/// A company number, as the admin page edits it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AdminNumber {
    /// The number (`PUT /v1/phone/admin/numbers/{id}`).
    pub id: i32,
    /// The number, E.164 (`+13175550101`).
    pub e164: String,
    /// Its label, e.g. "Main line"; may be empty.
    pub label: String,
    /// `direct` (one person's line), `shared` (a team line) or `main` (the
    /// company's main line, which rings only in business hours).
    pub kind: NumberKind,
    /// What a call nobody answers does: `ai` (the AI Assistant), `voicemail`
    /// or `forward` (to `forwardTo`).
    pub no_answer_action: NoAnswerAction,
    /// Where `forward` sends calls, E.164; null when none.
    pub forward_to: Option<String>,
    /// What callers hear before leaving a voicemail; null = the standard
    /// greeting.
    pub greeting: Option<String>,
    /// In use: an inactive number rings nobody and is nobody's caller ID.
    pub active: bool,
    /// Who it rings and who may call from it, by user id (users of the
    /// company only).
    pub assignments: Vec<NumberAssignment>,
}

#[derive(FromRow)]
struct NumberRow {
    id: i32,
    e164: String,
    label: String,
    #[sqlx(try_from = "String")]
    kind: NumberKind,
    #[sqlx(try_from = "String")]
    no_answer_action: NoAnswerAction,
    forward_to: Option<String>,
    greeting: Option<String>,
    active: bool,
}

#[derive(FromRow)]
struct AssignmentRow {
    number_id: i32,
    #[sqlx(flatten)]
    assignment: NumberAssignment,
}

/// A user of the company who may be given a number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct PhoneAdminUser {
    /// User id.
    pub id: i32,
    /// Their name, else their email.
    pub name: String,
}

/// The company's calling status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PhoneAdminStatus {
    /// Calling is switched on for the company (the superuser's kill switch).
    pub enabled: bool,
    /// Which calls are recorded: `off`, `inbound` or `both`.
    pub recording_mode: RecordingMode,
    /// Telnyx is set up for the company (its TeXML app and credential
    /// connection are saved): numbers can be imported, phones can call.
    pub configured: bool,
}

/// `GET /v1/phone/admin/numbers`.
#[derive(Debug, Serialize, ToSchema)]
pub struct AdminNumbers {
    /// The company's numbers: the main line first, then shared lines, then
    /// direct lines (each by label).
    pub numbers: Vec<AdminNumber>,
    /// The company's users who may use Granite Phone (employees, admins),
    /// by name: who numbers can be given to.
    pub users: Vec<PhoneAdminUser>,
    /// The company's calling status.
    pub config: PhoneAdminStatus,
}

/// Body of `PUT /v1/phone/admin/numbers/{id}`: the whole number.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NumberInput {
    /// Its label, up to 100 characters; may be empty.
    pub label: String,
    /// `direct`, `shared` or `main`.
    pub kind: NumberKind,
    /// `ai`, `voicemail` or `forward`.
    pub no_answer_action: NoAnswerAction,
    /// Where `forward` sends calls: a US or Canada number (E.164 or 10
    /// digits); needed for `forward`, null otherwise.
    #[serde(default)]
    pub forward_to: Option<String>,
    /// The voicemail greeting, up to 500 characters; null or empty = the
    /// standard greeting.
    #[serde(default)]
    pub greeting: Option<String>,
    /// In use.
    pub active: bool,
    /// Who it rings and who may call from it: replaces the current list.
    /// Every user must be a live user of the company, once each; users with
    /// neither flag on are left out.
    #[serde(default)]
    pub assignments: Vec<NumberAssignment>,
}

/// `POST /v1/phone/admin/numbers/import`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ImportedNumbers {
    /// How many numbers were added.
    pub imported: usize,
    /// The company's numbers now (as in `GET /v1/phone/admin/numbers`).
    pub numbers: Vec<AdminNumber>,
}

/// A phone signed in to the company.
#[derive(Debug, Serialize, FromRow, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AdminDevice {
    /// The device session: pass it to `DELETE /v1/phone/admin/devices/{id}`.
    pub id: i64,
    /// The name the phone gave at sign-in.
    pub device_name: String,
    /// `android` or `ios` (the app), or `web` (a browser phone of the CRM).
    #[sqlx(try_from = "String")]
    pub platform: DevicePlatform,
    /// The app version at sign-in; null when it sent none (always for a
    /// browser phone).
    pub app_version: Option<String>,
    /// When it signed in.
    #[serde(serialize_with = "js_date_required")]
    pub created_at: DateTime<Utc>,
    /// When it was last used (updated at most every 5 minutes); null when
    /// never since sign-in.
    #[serde(serialize_with = "js_date")]
    pub last_seen_at: Option<DateTime<Utc>>,
    /// The caller's own phone: always false here (the admin page signs in
    /// with the session cookie).
    pub current: bool,
    /// Whose phone it is.
    pub user_id: i32,
    /// Their name, else their email.
    pub user_name: String,
    /// The phone's SIP user on Telnyx (`gencred...`); null until it
    /// registered for calling with calling on.
    pub sip_username: Option<String>,
}

/// `GET /v1/phone/admin/devices`.
#[derive(Debug, Serialize, ToSchema)]
pub struct AdminDevices {
    /// The company's phones that are not signed out, newest first.
    pub devices: Vec<AdminDevice>,
}

/// The company's numbers with their assignments, as the page lists them.
pub(super) async fn company_numbers(
    pool: &MySqlPool,
    company_id: i32,
) -> sqlx::Result<Vec<AdminNumber>> {
    let rows: Vec<NumberRow> = sqlx::query_as(
        "SELECT id, e164, label, CAST(kind AS CHAR) AS kind,
                CAST(no_answer_action AS CHAR) AS no_answer_action, forward_to, greeting,
                active <> 0 AS active
           FROM telnyx_numbers WHERE company_id = ?
          ORDER BY FIELD(kind, 'main', 'shared', 'direct'), label, id",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    let assigned: Vec<AssignmentRow> = sqlx::query_as(
        "SELECT nu.number_id, nu.user_id, nu.rings <> 0 AS rings, nu.can_call_from <> 0 AS can_call_from
           FROM telnyx_number_users nu
           JOIN telnyx_numbers n ON n.id = nu.number_id
           JOIN users u ON u.id = nu.user_id AND u.company_id = n.company_id AND u.is_deleted = 0
          WHERE n.company_id = ?
          ORDER BY nu.number_id, nu.user_id",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    let mut by_number: HashMap<i32, Vec<NumberAssignment>> = HashMap::new();
    for row in assigned {
        by_number
            .entry(row.number_id)
            .or_default()
            .push(row.assignment);
    }
    Ok(rows
        .into_iter()
        .map(|row| AdminNumber {
            assignments: by_number.remove(&row.id).unwrap_or_default(),
            id: row.id,
            e164: row.e164,
            label: row.label,
            kind: row.kind,
            no_answer_action: row.no_answer_action,
            forward_to: row.forward_to,
            greeting: row.greeting,
            active: row.active,
        })
        .collect())
}

/// One number of the company.
pub(super) async fn company_number(
    pool: &MySqlPool,
    company_id: i32,
    id: i32,
) -> sqlx::Result<Option<AdminNumber>> {
    Ok(company_numbers(pool, company_id)
        .await?
        .into_iter()
        .find(|number| number.id == id))
}

/// List the company's phone numbers
///
/// The company's Granite Phone numbers (main line first, then shared and
/// direct lines), who each one rings and who may call from it; the users
/// numbers can be given to (employees and admins, by name); and the
/// company's calling status (`enabled`, `recordingMode`, `configured` =
/// Telnyx is set up). Change a number with `PUT
/// /v1/phone/admin/numbers/{id}`; add the TeXML app's numbers with `POST
/// /v1/phone/admin/numbers/import`. Company admins (and super admins,
/// superusers) only.
#[utoipa::path(
    get,
    path = "/v1/phone/admin/numbers",
    operation_id = "list_phone_admin_numbers",
    tag = "phone",
    responses(
        (status = 200, description = "The numbers, users and calling status", body = AdminNumbers),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_admin_numbers(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<AdminNumbers>> {
    let pool = &state.pool;
    let numbers = company_numbers(pool, user.company_id).await?;
    let users = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT u.id, COALESCE(NULLIF(TRIM(u.name), ''), u.email) AS name
           FROM users u
          WHERE u.company_id = ? AND u.is_deleted = 0 AND {PHONE_USER}
          ORDER BY name, u.id"
    )))
    .bind(user.company_id)
    .fetch_all(pool)
    .await?;
    let config = company::load(pool, user.company_id).await?;
    let config = PhoneAdminStatus {
        enabled: config.as_ref().is_some_and(|config| config.enabled),
        recording_mode: config
            .as_ref()
            .map_or(RecordingMode::Inbound, |config| config.recording_mode),
        configured: config.as_ref().is_some_and(|config| {
            config.texml_app_id.is_some() && config.credential_connection_id.is_some()
        }),
    };
    Ok(Json(AdminNumbers {
        numbers,
        users,
        config,
    }))
}

/// A number's fields after the checks.
struct CheckedNumber {
    label: String,
    forward_to: Option<String>,
    greeting: Option<String>,
    assignments: Vec<NumberAssignment>,
}

/// The input's fields as stored: 400 with a message for what is wrong.
fn check_number(input: &NumberInput) -> ApiResult<CheckedNumber> {
    let label = input.label.trim().to_string();
    if label.chars().count() > LABEL_CHARS {
        return Err(ApiError::BadRequest(format!(
            "Use at most {LABEL_CHARS} characters for the label."
        )));
    }
    let forward_to = match input.forward_to.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(number) => Some(
            to_e164(number)
                .filter(|number| is_nanp(number))
                .ok_or_else(|| {
                    ApiError::BadRequest("Forward to a US or Canada phone number.".into())
                })?,
        ),
    };
    if input.no_answer_action == NoAnswerAction::Forward && forward_to.is_none() {
        return Err(ApiError::BadRequest(
            "Enter the number to forward unanswered calls to.".into(),
        ));
    }
    let greeting = input
        .greeting
        .as_deref()
        .map(str::trim)
        .filter(|greeting| !greeting.is_empty())
        .map(str::to_string);
    if greeting
        .as_ref()
        .is_some_and(|greeting| greeting.chars().count() > GREETING_CHARS)
    {
        return Err(ApiError::BadRequest(format!(
            "Use at most {GREETING_CHARS} characters for the greeting."
        )));
    }
    if input.assignments.len() > MAX_ASSIGNMENTS {
        return Err(ApiError::BadRequest(format!(
            "A number can list at most {MAX_ASSIGNMENTS} people."
        )));
    }
    let mut seen = HashSet::new();
    if !input
        .assignments
        .iter()
        .all(|assignment| seen.insert(assignment.user_id))
    {
        return Err(ApiError::BadRequest("List each user once.".into()));
    }
    let assignments = input
        .assignments
        .iter()
        .filter(|assignment| assignment.rings || assignment.can_call_from)
        .cloned()
        .collect();
    Ok(CheckedNumber {
        label,
        forward_to,
        greeting,
        assignments,
    })
}

/// Whether every one of `user_ids` is a live user of the company.
async fn all_in_company(pool: &MySqlPool, company_id: i32, user_ids: &[i32]) -> sqlx::Result<bool> {
    if user_ids.is_empty() {
        return Ok(true);
    }
    let mut query = sqlx::QueryBuilder::new(
        "SELECT CAST(COUNT(*) AS SIGNED) FROM users WHERE is_deleted = 0 AND company_id = ",
    );
    query.push_bind(company_id);
    crate::sql::push_in(&mut query, "id", user_ids);
    let found: i64 = query.build_query_scalar().fetch_one(pool).await?;
    Ok(usize::try_from(found).is_ok_and(|found| found == user_ids.len()))
}

/// Change a phone number
///
/// Saves the whole number: label, kind (`direct`, `shared`, `main`), what a
/// call nobody answers does (`ai`, `voicemail`, `forward` with `forwardTo`,
/// a US or Canada number), the voicemail greeting, whether it is active, and
/// who it rings and who may call from it (the list replaces the current
/// one; every user must be a live user of the company, else 400). Returns
/// the number as saved. The change is written to the phone audit log.
/// Company admins (and super admins, superusers) only.
#[utoipa::path(
    put,
    path = "/v1/phone/admin/numbers/{id}",
    operation_id = "update_phone_admin_number",
    tag = "phone",
    params(("id" = i32, Path, description = "The number (`AdminNumber.id`)")),
    request_body = NumberInput,
    responses(
        (status = 200, description = "The number as saved", body = AdminNumber),
        (status = 400, description = "What is wrong: a label or greeting too long, no or a bad forward number, a user who is not in the company", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such number in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_admin_number(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<NumberInput>,
) -> ApiResult<Json<AdminNumber>> {
    let pool = &state.pool;
    let checked = check_number(&input)?;
    let before = company_number(pool, user.company_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let user_ids: Vec<i32> = checked
        .assignments
        .iter()
        .map(|assignment| assignment.user_id)
        .collect();
    if !all_in_company(pool, user.company_id, &user_ids).await? {
        return Err(ApiError::BadRequest(
            "Every user must be a user of your company.".into(),
        ));
    }
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE telnyx_numbers
            SET label = ?, kind = ?, no_answer_action = ?, forward_to = ?, greeting = ?, active = ?
          WHERE id = ? AND company_id = ?",
    )
    .bind(&checked.label)
    .bind(input.kind.as_str())
    .bind(input.no_answer_action.as_str())
    .bind(&checked.forward_to)
    .bind(&checked.greeting)
    .bind(input.active)
    .bind(id)
    .bind(user.company_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM telnyx_number_users WHERE number_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    for assignment in &checked.assignments {
        sqlx::query(
            "INSERT INTO telnyx_number_users (number_id, user_id, rings, can_call_from)
             VALUES (?, ?, ?, ?)",
        )
        .bind(id)
        .bind(assignment.user_id)
        .bind(assignment.rings)
        .bind(assignment.can_call_from)
        .execute(&mut *tx)
        .await?;
    }
    let after = json!({
        "label": checked.label, "kind": input.kind, "noAnswerAction": input.no_answer_action,
        "forwardTo": checked.forward_to, "greeting": checked.greeting, "active": input.active,
        "assignments": checked.assignments,
    });
    audit(
        &mut *tx,
        user.company_id,
        user.id,
        "number_updated",
        &format!("number:{id}"),
        &json!({ "before": before, "after": after }),
    )
    .await?;
    tx.commit().await?;
    let saved = company_number(pool, user.company_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(saved))
}

/// Import the company's numbers from Telnyx
///
/// Reads the numbers assigned to the company's TeXML app in Telnyx and adds
/// the ones that are missing here as unassigned direct lines (no label,
/// the AI Assistant when nobody answers); numbers already here are left as
/// they are. Returns how many were added and the company's numbers. 400
/// when Telnyx is not set up for the company (no TeXML app saved), 503
/// when Telnyx is not set up on the server, 502 when Telnyx fails. Company
/// admins (and super admins, superusers) only.
#[utoipa::path(
    post,
    path = "/v1/phone/admin/numbers/import",
    operation_id = "import_phone_admin_numbers",
    tag = "phone",
    responses(
        (status = 200, description = "How many were added, and the numbers now", body = ImportedNumbers),
        (status = 400, description = "Telnyx is not set up for the company", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "Telnyx failed; try again", body = ErrorBody),
        (status = 503, description = "Telnyx is not set up on the server", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn import_admin_numbers(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<ImportedNumbers>> {
    let app_id = company::load(&state.pool, user.company_id)
        .await?
        .and_then(|config| config.texml_app_id)
        .ok_or_else(|| {
            ApiError::BadRequest(
                "Telnyx is not connected for this company yet. Click Connect Telnyx first.".into(),
            )
        })?;
    let imported = import_numbers(&state, user.company_id, user.id, &app_id).await?;
    Ok(Json(ImportedNumbers {
        imported,
        numbers: company_numbers(&state.pool, user.company_id).await?,
    }))
}

/// Adds the numbers on the company's TeXML app (`app_id`) that are missing
/// here, as unassigned direct lines; how many were added. Numbers already
/// here keep their settings (and get their Telnyx id when they had none);
/// numbers another company has are left out.
pub(super) async fn import_numbers(
    state: &AppState,
    company_id: i32,
    actor: i32,
    app_id: &str,
) -> ApiResult<usize> {
    let pool = &state.pool;
    let listed = state
        .telnyx
        .list_numbers_for_connection(app_id)
        .await
        .map_err(|err| telnyx_failed(&err, "list the TeXML app's numbers"))?;
    let mut wanted: Vec<(String, String, String)> = Vec::new();
    for number in &listed {
        let Some(e164) = to_e164(&number.phone_number) else {
            continue;
        };
        let Some(digits) = last10(&e164) else {
            continue;
        };
        if !wanted.iter().any(|(known, _, _)| *known == e164) {
            let telnyx_id: String = number.id.chars().take(64).collect();
            wanted.push((e164, digits, telnyx_id));
        }
    }
    let mut imported = 0;
    let mut elsewhere = 0;
    let mut tx = pool.begin().await?;
    for (e164, digits, telnyx_id) in &wanted {
        let owner: Option<i32> =
            sqlx::query_scalar("SELECT company_id FROM telnyx_numbers WHERE e164 = ?")
                .bind(e164)
                .fetch_optional(&mut *tx)
                .await?;
        match owner {
            Some(owner) if owner == company_id => {
                sqlx::query(
                    "UPDATE telnyx_numbers SET telnyx_number_id = ?
                      WHERE e164 = ? AND company_id = ? AND telnyx_number_id IS NULL",
                )
                .bind(telnyx_id)
                .bind(e164)
                .bind(company_id)
                .execute(&mut *tx)
                .await?;
            }
            Some(_) => elsewhere += 1,
            None => {
                imported += sqlx::query(
                    "INSERT IGNORE INTO telnyx_numbers
                       (company_id, e164, last10, telnyx_number_id, label, kind)
                     VALUES (?, ?, ?, ?, '', 'direct')",
                )
                .bind(company_id)
                .bind(e164)
                .bind(digits)
                .bind(telnyx_id)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            }
        }
    }
    if imported > 0 {
        audit(
            &mut *tx,
            company_id,
            actor,
            "numbers_imported",
            &format!("texml_app:{app_id}"),
            &json!({ "imported": imported }),
        )
        .await?;
    }
    tx.commit().await?;
    if elsewhere > 0 {
        tracing::warn!(
            company_id,
            numbers = elsewhere,
            "TeXML app numbers belong to another company; not imported"
        );
    }
    Ok(usize::try_from(imported).unwrap_or(usize::MAX))
}

/// List the company's signed-in phones
///
/// Every Granite Phone sign-in of the company's users that is not signed
/// out (expired and idle ones stay listed until revoked), newest first,
/// with whose phone it is and its Telnyx SIP user. Sign one out with
/// `DELETE /v1/phone/admin/devices/{id}`. Company admins (and super admins,
/// superusers) only.
#[utoipa::path(
    get,
    path = "/v1/phone/admin/devices",
    operation_id = "list_phone_admin_devices",
    tag = "phone",
    responses(
        (status = 200, description = "The company's phones", body = AdminDevices),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_admin_devices(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<AdminDevices>> {
    let devices = sqlx::query_as(
        "SELECT CAST(ds.id AS SIGNED) AS id, ds.device_name, CAST(ds.platform AS CHAR) AS platform,
                ds.app_version, ds.created_at, ds.last_seen_at, FALSE AS current, ds.user_id,
                COALESCE(NULLIF(TRIM(u.name), ''), u.email) AS user_name, pd.sip_username
           FROM device_sessions ds
           JOIN users u ON u.id = ds.user_id
           LEFT JOIN phone_devices pd ON pd.device_session_id = ds.id AND pd.revoked_at IS NULL
          WHERE ds.company_id = ? AND ds.revoked_at IS NULL
          ORDER BY ds.created_at DESC, ds.id DESC",
    )
    .bind(user.company_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(AdminDevices { devices }))
}

/// Sign a company phone out
///
/// Revokes a device session of the company (an id from
/// `GET /v1/phone/admin/devices`): its token stops working at once, the
/// phone is told to wipe itself (a `revoked` push), its Telnyx credential
/// is deleted (retried by the phone job when Telnyx fails) and its push
/// token cleared. 404 when it is not the company's or is signed out
/// already. Written to the phone audit log. Company admins (and super
/// admins, superusers) only.
#[utoipa::path(
    delete,
    path = "/v1/phone/admin/devices/{id}",
    operation_id = "revoke_phone_admin_device",
    tag = "phone",
    params(("id" = i64, Path, description = "Device session id (`AdminDevice.id`)")),
    responses(
        (status = 204, description = "Signed out"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "Not a signed-in phone of the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn revoke_admin_device(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    let owner: Option<i32> = sqlx::query_scalar(
        "SELECT user_id FROM device_sessions WHERE id = ? AND company_id = ? AND revoked_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_optional(&state.pool)
    .await?;
    let Some(owner) = owner else {
        return Err(ApiError::NotFound);
    };
    if !revoke_device_session(&state, id, RevokeReason::ByAdmin).await? {
        return Err(ApiError::NotFound);
    }
    audit(
        &state.pool,
        user.company_id,
        user.id,
        "device_revoked",
        &format!("device_session:{id}"),
        &json!({ "userId": owner }),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// When the company's main line rings: `GET` and `PUT /v1/phone/admin/hours`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CompanyHours {
    /// The main line's hours (a valid IANA `tz`, at least one range, `HH:MM`
    /// with start before end); null = always.
    #[serde(default)]
    pub business_hours: Option<WorkingHours>,
    /// Dates the main line rings nobody, `YYYY-MM-DD` in the company time
    /// zone (at most 366; sorted, repeats dropped).
    #[serde(default)]
    pub holidays: Vec<String>,
}

/// Get the company's business hours
///
/// The main line's business hours and holidays. 404 until Telnyx is
/// connected for the company. Company admins (and super admins,
/// superusers) only.
#[utoipa::path(
    get,
    path = "/v1/phone/admin/hours",
    operation_id = "get_phone_admin_hours",
    tag = "phone",
    responses(
        (status = 200, description = "The hours and holidays", body = CompanyHours),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "Telnyx is not set up for the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_admin_hours(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<CompanyHours>> {
    let config = company::load(&state.pool, user.company_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(CompanyHours {
        business_hours: config.business_hours,
        holidays: config.holidays,
    }))
}

/// Save the company's business hours
///
/// Saves the main line's business hours and holidays and returns them.
/// Checks: a valid IANA time zone, at least one range, `HH:MM` with start
/// before end, `YYYY-MM-DD` holidays (at most 366); 400 with a message
/// otherwise. 404 until Telnyx is connected for the company.
/// Written to the phone audit log. Company admins (and super admins,
/// superusers) only.
#[utoipa::path(
    put,
    path = "/v1/phone/admin/hours",
    operation_id = "save_phone_admin_hours",
    tag = "phone",
    request_body = CompanyHours,
    responses(
        (status = 200, description = "The hours as saved", body = CompanyHours),
        (status = 400, description = "What is wrong", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "Telnyx is not set up for the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn save_admin_hours(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<CompanyHours>,
) -> ApiResult<Json<CompanyHours>> {
    let hours = CompanyHours {
        business_hours: checked_hours(input.business_hours)?,
        holidays: checked_holidays(&input.holidays)?,
    };
    if company::load(&state.pool, user.company_id).await?.is_none() {
        return Err(ApiError::NotFound);
    }
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "UPDATE telnyx_company_config SET business_hours = ?, holidays = ? WHERE company_id = ?",
    )
    .bind(hours.business_hours.as_ref().map(WorkingHours::to_json))
    .bind(serde_json::to_string(&hours.holidays).unwrap_or_else(|_| "[]".into()))
    .bind(user.company_id)
    .execute(&mut *tx)
    .await?;
    audit(
        &mut *tx,
        user.company_id,
        user.id,
        "business_hours_saved",
        &format!("company:{}", user.company_id),
        &serde_json::to_value(&hours).unwrap_or_default(),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(hours))
}

/// Which calls the company records: `GET` and `PUT /v1/phone/admin/recording`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CompanyRecording {
    /// `off`, `inbound` (incoming calls) or `both` (incoming and outgoing).
    /// Callers hear "This call may be recorded" before it rings.
    pub recording_mode: RecordingMode,
    /// The notice played to the customer on recorded outgoing calls, an
    /// audio file URL (`http(s)://`, at most 512 characters); null or blank
    /// = "This call may be recorded." is said instead.
    #[serde(default)]
    pub recording_notice_url: Option<String>,
}

/// Get the company's call recording
///
/// Which calls are recorded and the notice played on recorded outgoing
/// calls. 404 until Telnyx is connected for the company. Company
/// admins (and super admins, superusers) only.
#[utoipa::path(
    get,
    path = "/v1/phone/admin/recording",
    operation_id = "get_phone_admin_recording",
    tag = "phone",
    responses(
        (status = 200, description = "The recording setting", body = CompanyRecording),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "Telnyx is not set up for the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_admin_recording(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<CompanyRecording>> {
    let config = company::load(&state.pool, user.company_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(CompanyRecording {
        recording_mode: config.recording_mode,
        recording_notice_url: config.recording_notice_url,
    }))
}

/// Save the company's call recording
///
/// Saves which calls are recorded and the notice audio, and returns them.
/// The notice is an `http(s)://` URL (at most 512 characters; blank =
/// spoken); 400 otherwise. 404 until Telnyx is connected for the
/// company. Written to the phone audit log. Company admins (and super
/// admins, superusers) only.
#[utoipa::path(
    put,
    path = "/v1/phone/admin/recording",
    operation_id = "save_phone_admin_recording",
    tag = "phone",
    request_body = CompanyRecording,
    responses(
        (status = 200, description = "The recording setting as saved", body = CompanyRecording),
        (status = 400, description = "What is wrong", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "Telnyx is not set up for the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn save_admin_recording(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<CompanyRecording>,
) -> ApiResult<Json<CompanyRecording>> {
    let recording = CompanyRecording {
        recording_mode: input.recording_mode,
        recording_notice_url: http_url(input.recording_notice_url.as_ref(), "recording notice")?,
    };
    if company::load(&state.pool, user.company_id).await?.is_none() {
        return Err(ApiError::NotFound);
    }
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "UPDATE telnyx_company_config SET recording_mode = ?, recording_notice_url = ?
          WHERE company_id = ?",
    )
    .bind(recording.recording_mode.as_str())
    .bind(&recording.recording_notice_url)
    .bind(user.company_id)
    .execute(&mut *tx)
    .await?;
    audit(
        &mut *tx,
        user.company_id,
        user.id,
        "recording_saved",
        &format!("company:{}", user.company_id),
        &serde_json::to_value(&recording).unwrap_or_default(),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(recording))
}
