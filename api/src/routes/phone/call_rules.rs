//! My incoming call rules ("Incoming call rules" → "+ Add rule"):
//! `/v1/phone/call-rules`. What they do is `phone::call_rules`.

use super::bad_request;
use crate::auth::device::DeviceUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::phone::availability::WorkingHours;
use crate::phone::call_rules::{
    self, CallRule, MAX_CALLERS, MAX_MESSAGE_CHARS, MAX_NAME_CHARS, MAX_RULES, RuleAction,
    RuleCaller,
};
use crate::phone::company;
use crate::phone::lines::NumberKind;
use crate::phone::numbers::to_e164;
use crate::phone::routing::{self, NoAnswer};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::{MySqlPool, QueryBuilder};

/// `GET /v1/phone/call-rules`.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CallRules {
    /// My rules, in the order they are tried (the first that matches an
    /// incoming call decides).
    pub rules: Vec<CallRule>,
}

/// Body of `POST /v1/phone/call-rules` and `PUT /v1/phone/call-rules/{id}`:
/// the whole rule.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuleInput {
    /// Its name, 1 to 100 characters.
    pub name: String,
    /// The rule's switch (default on).
    #[serde(default = "on")]
    pub enabled: bool,
    /// The callers it is for, each a `number` (any US or Canada format, or
    /// E.164) or a `customerId` of the company; at most 50. Null or empty =
    /// any caller.
    #[serde(default)]
    pub callers: Option<Vec<RuleCaller>>,
    /// When it applies (weekly local time ranges, like working hours);
    /// null = any time.
    #[serde(default)]
    pub schedule: Option<WorkingHours>,
    /// Which of my numbers was called: ids of numbers that ring me or that I
    /// may call from (400 `not_my_number` otherwise). Null or empty = any.
    #[serde(default)]
    pub number_ids: Option<Vec<i32>>,
    /// `ring`, `forward`, `voicemail` or `message`.
    pub action: RuleAction,
    /// With `forward`: a US or Canada number (400 `invalid_phone`).
    #[serde(default)]
    pub forward_to: Option<String>,
    /// With `message`: what the caller hears, 1 to 500 characters.
    #[serde(default)]
    pub message: Option<String>,
}

const fn on() -> bool {
    true
}

/// A rule as checked, ready to store.
struct CheckedRule {
    name: String,
    enabled: bool,
    callers: Option<Vec<RuleCaller>>,
    schedule: Option<WorkingHours>,
    number_ids: Option<Vec<i32>>,
    action: RuleAction,
    forward_to: Option<String>,
    message: Option<String>,
}

/// Checks a rule of `user_id` (ids scoped to the company); 400 with what is
/// wrong.
#[allow(clippy::too_many_lines)]
async fn check(
    pool: &MySqlPool,
    company_id: i32,
    user_id: i32,
    input: RuleInput,
) -> ApiResult<CheckedRule> {
    let name = input.name.trim().to_string();
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
        return Err(ApiError::BadRequest(
            "Give the rule a name of at most 100 characters.".into(),
        ));
    }
    let callers = input.callers.filter(|callers| !callers.is_empty());
    let callers = match callers {
        None => None,
        Some(callers) if callers.len() > MAX_CALLERS => {
            return Err(ApiError::BadRequest(
                "A rule names at most 50 callers.".into(),
            ));
        }
        Some(callers) => {
            let mut checked = Vec::with_capacity(callers.len());
            let mut customers = Vec::new();
            for caller in callers {
                match (caller.number.as_deref(), caller.customer_id) {
                    (Some(number), None) => checked.push(RuleCaller {
                        number: Some(to_e164(number).ok_or_else(|| bad_request("invalid_phone"))?),
                        customer_id: None,
                    }),
                    (None, Some(id)) => {
                        customers.push(id);
                        checked.push(RuleCaller {
                            number: None,
                            customer_id: Some(id),
                        });
                    }
                    _ => {
                        return Err(ApiError::BadRequest(
                            "Each caller is a number or a contact.".into(),
                        ));
                    }
                }
            }
            customers.sort_unstable();
            customers.dedup();
            if !customers.is_empty() {
                let mut query = QueryBuilder::new(
                    "SELECT COUNT(*) FROM customers WHERE deleted_at IS NULL AND company_id = ",
                );
                query.push_bind(company_id);
                crate::sql::push_in(&mut query, "id", &customers);
                let found: i64 = query.build_query_scalar().fetch_one(pool).await?;
                if usize::try_from(found).ok() != Some(customers.len()) {
                    return Err(bad_request("unknown_contact"));
                }
            }
            Some(checked)
        }
    };
    if let Some(problem) = input.schedule.as_ref().and_then(WorkingHours::problem) {
        return Err(ApiError::BadRequest(problem));
    }
    let schedule = input.schedule.map(|hours| WorkingHours {
        tz: hours.tz.trim().to_string(),
        days: hours.days,
    });
    let mut number_ids = input.number_ids.filter(|ids| !ids.is_empty());
    if let Some(ids) = number_ids.as_mut() {
        ids.sort_unstable();
        ids.dedup();
        let mut query = QueryBuilder::new(
            "SELECT COUNT(*) FROM telnyx_numbers n
               JOIN telnyx_number_users nu ON nu.number_id = n.id AND nu.user_id = ",
        );
        query
            .push_bind(user_id)
            .push(" AND (nu.rings <> 0 OR nu.can_call_from <> 0) WHERE n.company_id = ")
            .push_bind(company_id);
        crate::sql::push_in(&mut query, "n.id", ids);
        let found: i64 = query.build_query_scalar().fetch_one(pool).await?;
        if usize::try_from(found).ok() != Some(ids.len()) {
            return Err(bad_request("not_my_number"));
        }
    }
    let forward_to = match input.action {
        RuleAction::Forward => Some(
            call_rules::forward_number(input.forward_to.as_deref())
                .ok_or_else(|| bad_request("invalid_phone"))?,
        ),
        _ => None,
    };
    let message = match input.action {
        RuleAction::Message => {
            let message = input.message.as_deref().map(str::trim).unwrap_or_default();
            if message.is_empty() || message.chars().count() > MAX_MESSAGE_CHARS {
                return Err(ApiError::BadRequest(
                    "Write a message of at most 500 characters.".into(),
                ));
            }
            Some(message.to_string())
        }
        _ => None,
    };
    Ok(CheckedRule {
        name,
        enabled: input.enabled,
        callers,
        schedule,
        number_ids,
        action: input.action,
        forward_to,
        message,
    })
}

/// A company number that rings me or that I may call from.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MyLine {
    /// The number's id (for a rule's `numberIds`).
    pub number_id: i32,
    /// The number, E.164.
    pub e164: String,
    /// Its name, e.g. "Sales"; may be empty.
    pub label: String,
    /// `direct`, `shared` or `main`.
    pub kind: NumberKind,
    /// Calls to it ring my phones (else I only call from it).
    pub rings: bool,
    /// It rings nobody but me: my rules and "Forward all calls" decide its
    /// calls whole.
    pub mine_alone: bool,
    /// What its calls do when nobody answers, as it works now: `ai` (the
    /// AI Assistant), `voicemail` or `forward`.
    pub no_answer: String,
}

/// `GET /v1/phone/lines`.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MyLines {
    /// My lines: direct first, then shared, then the main line.
    pub lines: Vec<MyLine>,
}

#[derive(sqlx::FromRow)]
struct MyLineRow {
    id: i32,
    rings: bool,
    ringers: i64,
}

/// List my lines
///
/// The company's active numbers that ring me or that I may call from, with
/// what their calls do when nobody answers (the AI Assistant, voicemail or
/// a forward, as it works now: the AI Assistant falls back to voicemail
/// when the company has none) and whether the line rings only me (where my
/// rules decide whole calls). For the "Incoming calls" summary and a rule's
/// "number called" condition. Device token only.
#[crate::openapi::path(
    get,
    path = "/v1/phone/lines",
    operation_id = "list_phone_lines",
    tag = "phone",
    responses(
        (status = 200, description = "My lines", body = MyLines),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn list_lines(
    State(state): State<AppState>,
    device: DeviceUser,
) -> ApiResult<Json<MyLines>> {
    let user = &device.user;
    let pool = &state.pool;
    let rows: Vec<MyLineRow> = sqlx::query_as(
        "SELECT n.id, nu.rings <> 0 AS rings,
                (SELECT COUNT(*) FROM telnyx_number_users o
                   JOIN users ou ON ou.id = o.user_id AND ou.company_id = n.company_id
                                AND ou.is_deleted = 0
                  WHERE o.number_id = n.id AND o.rings <> 0) AS ringers
           FROM telnyx_numbers n
           JOIN telnyx_number_users nu ON nu.number_id = n.id AND nu.user_id = ?
                AND (nu.rings <> 0 OR nu.can_call_from <> 0)
          WHERE n.company_id = ? AND n.active <> 0
          ORDER BY FIELD(n.kind, 'direct', 'shared', 'main'), n.label, n.id",
    )
    .bind(user.id)
    .bind(user.company_id)
    .fetch_all(pool)
    .await?;
    let config = company::load(pool, user.company_id).await?;
    let mut lines = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(line) = routing::line(pool, user.company_id, row.id).await? else {
            continue;
        };
        let no_answer = match &config {
            Some(config) => match routing::no_answer(&line, config, Vec::new(), true, false) {
                NoAnswer::Ai(_) => "ai",
                NoAnswer::Forward(_) => "forward",
                _ => "voicemail",
            },
            None => line.no_answer_action.as_str(),
        };
        lines.push(MyLine {
            number_id: line.id,
            mine_alone: line.kind == NumberKind::Direct && row.rings && row.ringers == 1,
            e164: line.e164,
            label: line.label,
            kind: line.kind,
            rings: row.rings,
            no_answer: no_answer.to_string(),
        });
    }
    Ok(Json(MyLines { lines }))
}

/// A JSON column's value (NULL for none).
fn json<T: Serialize>(value: Option<&T>) -> Option<String> {
    value.and_then(|value| serde_json::to_string(value).ok())
}

/// One of my rules; 404 when it is not mine.
async fn my_rule(pool: &MySqlPool, user: &DeviceUser, id: i64) -> ApiResult<CallRule> {
    call_rules::list(pool, user.user.company_id, user.user.id)
        .await?
        .into_iter()
        .find(|rule| rule.id == id)
        .ok_or(ApiError::NotFound)
}

/// List my incoming call rules
///
/// My custom incoming call rules, in the order they are tried. "Forward all
/// calls", Do not disturb and working hours are in `GET /v1/phone/bootstrap`
/// `settings` / `availability`. Device token only.
#[crate::openapi::path(
    get,
    path = "/v1/phone/call-rules",
    operation_id = "list_phone_call_rules",
    tag = "phone",
    responses(
        (status = 200, description = "My rules", body = CallRules),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn list_call_rules(
    State(state): State<AppState>,
    device: DeviceUser,
) -> ApiResult<Json<CallRules>> {
    let user = &device.user;
    Ok(Json(CallRules {
        rules: call_rules::list(&state.pool, user.company_id, user.id).await?,
    }))
}

/// Add an incoming call rule
///
/// A new rule, tried after my others: when a call matches every condition
/// it sets (callers, schedule, number called), it decides what the call
/// does for me (ring my phones even outside working hours, forward, send
/// to voicemail, or play a message and hang up). On my own direct line it
/// decides the whole call; on shared and main lines only whether my phones
/// ring. Do not disturb and "Forward all calls" win over rules. At most 20
/// rules (400 `too_many_rules`). 400 `invalid_phone`, `unknown_contact`,
/// `not_my_number`, or what is wrong. Device token only.
#[crate::openapi::path(
    post,
    path = "/v1/phone/call-rules",
    operation_id = "create_phone_call_rule",
    tag = "phone",
    request_body = RuleInput,
    responses(
        (status = 201, description = "The new rule", body = CallRule),
        (status = 400, description = "`too_many_rules`, `invalid_phone`, `unknown_contact`, `not_my_number`, or what is wrong", body = ErrorBody),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn create_call_rule(
    State(state): State<AppState>,
    device: DeviceUser,
    JsonBody(input): JsonBody<RuleInput>,
) -> ApiResult<(StatusCode, Json<CallRule>)> {
    let user = &device.user;
    let pool = &state.pool;
    let rule = check(pool, user.company_id, user.id, input).await?;
    let mut tx = pool.begin().await?;
    // The user's settings row orders two requests of the same user.
    crate::phone::settings::ensure_row(&mut tx, user.id, user.company_id).await?;
    let (count, last): (i64, Option<i32>) = sqlx::query_as(
        "SELECT COUNT(*), MAX(position) FROM phone_call_rules WHERE company_id = ? AND user_id = ?",
    )
    .bind(user.company_id)
    .bind(user.id)
    .fetch_one(&mut *tx)
    .await?;
    if usize::try_from(count).unwrap_or(usize::MAX) >= MAX_RULES {
        return Err(bad_request("too_many_rules"));
    }
    let id = sqlx::query(
        "INSERT INTO phone_call_rules
           (company_id, user_id, name, enabled, position, callers, schedule, number_ids,
            action, forward_to, message)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(user.company_id)
    .bind(user.id)
    .bind(&rule.name)
    .bind(rule.enabled)
    .bind(last.map_or(0, |last| last.saturating_add(1)))
    .bind(json(rule.callers.as_ref()))
    .bind(rule.schedule.as_ref().map(WorkingHours::to_json))
    .bind(json(rule.number_ids.as_ref()))
    .bind(rule.action.as_str())
    .bind(&rule.forward_to)
    .bind(&rule.message)
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    tx.commit().await?;
    let id = i64::try_from(id).unwrap_or(i64::MAX);
    Ok((StatusCode::CREATED, Json(my_rule(pool, &device, id).await?)))
}

/// Change an incoming call rule
///
/// Replaces my rule with the one sent (send it whole, e.g. with `enabled`
/// flipped for its switch); it keeps its place in the order. 404 when the
/// rule is not mine; 400 as for `POST /v1/phone/call-rules`. Device token
/// only.
#[crate::openapi::path(
    put,
    path = "/v1/phone/call-rules/{id}",
    operation_id = "update_phone_call_rule",
    tag = "phone",
    params(("id" = i64, Path, description = "The rule's id")),
    request_body = RuleInput,
    responses(
        (status = 200, description = "The rule now", body = CallRule),
        (status = 400, description = "`invalid_phone`, `unknown_contact`, `not_my_number`, or what is wrong", body = ErrorBody),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 404, description = "Not one of my rules", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn update_call_rule(
    State(state): State<AppState>,
    device: DeviceUser,
    Path(id): Path<i64>,
    JsonBody(input): JsonBody<RuleInput>,
) -> ApiResult<Json<CallRule>> {
    let user = &device.user;
    let pool = &state.pool;
    my_rule(pool, &device, id).await?;
    let rule = check(pool, user.company_id, user.id, input).await?;
    sqlx::query(
        "UPDATE phone_call_rules
            SET name = ?, enabled = ?, callers = ?, schedule = ?, number_ids = ?, action = ?,
                forward_to = ?, message = ?
          WHERE id = ? AND company_id = ? AND user_id = ?",
    )
    .bind(&rule.name)
    .bind(rule.enabled)
    .bind(json(rule.callers.as_ref()))
    .bind(rule.schedule.as_ref().map(WorkingHours::to_json))
    .bind(json(rule.number_ids.as_ref()))
    .bind(rule.action.as_str())
    .bind(&rule.forward_to)
    .bind(&rule.message)
    .bind(id)
    .bind(user.company_id)
    .bind(user.id)
    .execute(pool)
    .await?;
    Ok(Json(my_rule(pool, &device, id).await?))
}

/// Delete an incoming call rule
///
/// Deletes my rule; 404 when it is not mine. Device token only.
#[crate::openapi::path(
    delete,
    path = "/v1/phone/call-rules/{id}",
    operation_id = "delete_phone_call_rule",
    tag = "phone",
    params(("id" = i64, Path, description = "The rule's id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 404, description = "Not one of my rules", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn delete_call_rule(
    State(state): State<AppState>,
    device: DeviceUser,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    let user = &device.user;
    let deleted =
        sqlx::query("DELETE FROM phone_call_rules WHERE id = ? AND company_id = ? AND user_id = ?")
            .bind(id)
            .bind(user.company_id)
            .bind(user.id)
            .execute(&state.pool)
            .await?
            .rows_affected();
    if deleted == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}
