//! Sends the due steps of SMS / email flows (`sms_flow_enrollments`).
//!
//! Each due enrollment is claimed (its `next_send_at` pushed out 20 minutes)
//! so overlapping runs never send twice. Before sending: a CRM or customer
//! hold reschedules it, a pending phone call the customer asked for blocks it
//! (rechecked daily), and any reply, email or incoming call since the start
//! stops it. AI steps are written then; template steps fill their
//! variables. A refused send is retried twice (then the run fails); an AI
//! step the writer skips completes the run. Port of
//! `smsFlowProcessor.server.ts`.

use super::classify::SMS_FLOW_AI_SENTINEL;
use super::crm_hold::must_reschedule_for_hold;
use super::dates::Instant;
use super::facts::{ManualActionRequired, ManualActionStatus};
use super::gather::{FollowupRequest, gather_followup_context};
use super::generation::{Writer, complete_followup};
use super::prompt::GeneratedFollowup;
use super::send_window::{
    FollowupChannel, clamp_followup_send_at, followup_next_send_at, is_followup_send_window_open,
    next_send_at_avoiding_sunday,
};
use crate::auth::session::{SessionUser, find_user};
use crate::openapi::ToSchema;
use crate::routes::emails::send::{Cleaned, deliver};
use crate::routes::sms::provider::SmsProvider;
use crate::state::AppState;
use crate::webhooks_client::OutgoingSms;
use chrono::{Duration, NaiveDateTime, Utc};
use lambda_http::tracing;
use serde::Serialize;

const DISPATCH_BATCH_LIMIT: i64 = 50;
/// One company's backlog cannot fill the whole batch.
const PER_COMPANY_BATCH_LIMIT: i64 = 10;
const MAX_SEND_ATTEMPTS: i32 = 3;
const CLAIM_WINDOW_MINUTES: i64 = 20;
/// A customer-requested phone call is rechecked daily until it happened.
const MANUAL_ACTION_RECHECK_MINUTES: i64 = 24 * 60;

/// What a run did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub struct FollowupRun {
    /// Enrollments looked at.
    pub processed: usize,
    pub sent: usize,
    pub failed: usize,
    /// Stopped because the customer replied or called.
    pub stopped: usize,
}

#[derive(sqlx::FromRow, Debug, Clone)]
struct Due {
    id: i32,
    flow_id: i32,
    company_id: i32,
    customer_id: Option<i32>,
    user_id: i32,
    current_step_position: i32,
    attempt_count: i32,
    started_at: NaiveDateTime,
    phone_digits: Option<String>,
    customer_email: Option<String>,
}

const DUE_COLUMNS: &str = "e.id, e.flow_id, e.company_id, e.customer_id, e.user_id, e.current_step_position, e.attempt_count,
    CAST(e.started_at AS DATETIME) AS started_at, CAST(e.customer_phone_digits AS CHAR) AS phone_digits, e.customer_email";

/// Due by the run's clock (a test's, or now), like every send time it sets.
async fn due_enrollments(state: &AppState, now: Instant) -> Result<Vec<Due>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, flow_id, company_id, customer_id, user_id, current_step_position, attempt_count, started_at,
                phone_digits, customer_email
           FROM (
             SELECT e.id, e.flow_id, e.company_id, e.customer_id, e.user_id, e.current_step_position, e.attempt_count,
                    CAST(e.started_at AS DATETIME) AS started_at, CAST(e.customer_phone_digits AS CHAR) AS phone_digits,
                    e.customer_email, e.next_send_at,
                    ROW_NUMBER() OVER (PARTITION BY e.company_id ORDER BY e.next_send_at ASC) AS rn
               FROM sms_flow_enrollments e
               JOIN sms_flows f ON f.id = e.flow_id AND f.deleted_at IS NULL AND f.company_id = e.company_id
              WHERE e.status = 'active' AND e.next_send_at <= ?
           ) ranked
          WHERE rn <= ?
          ORDER BY next_send_at ASC
          LIMIT ?",
    )
    .bind(now.naive_utc())
    .bind(PER_COMPANY_BATCH_LIMIT)
    .bind(DISPATCH_BATCH_LIMIT)
    .fetch_all(&state.pool)
    .await
}

async fn due_enrollment(
    state: &AppState,
    id: i32,
    now: Instant,
) -> Result<Option<Due>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {DUE_COLUMNS} FROM sms_flow_enrollments e
           JOIN sms_flows f ON f.id = e.flow_id AND f.deleted_at IS NULL AND f.company_id = e.company_id
          WHERE e.id = ? AND e.status = 'active' AND e.next_send_at <= ? LIMIT 1"
    )))
    .bind(id)
    .bind(now.naive_utc())
    .fetch_optional(&state.pool)
    .await
}

/// Never overwrites a stop that landed meanwhile; whether it changed.
async fn fail(state: &AppState, id: i32, token: &str) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query("UPDATE sms_flow_enrollments SET status = 'failed', error_message = ?, next_send_at = NULL WHERE id = ? AND status = 'active'")
        .bind(token)
        .bind(id)
        .execute(&state.pool)
        .await?
        .rows_affected()
        == 1)
}

async fn claim(state: &AppState, id: i32) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query(
        "UPDATE sms_flow_enrollments SET next_send_at = DATE_ADD(UTC_TIMESTAMP(), INTERVAL ? MINUTE)
          WHERE id = ? AND status = 'active' AND next_send_at <= UTC_TIMESTAMP()",
    )
    .bind(CLAIM_WINDOW_MINUTES)
    .bind(id)
    .execute(&state.pool)
    .await?
    .rows_affected()
        == 1)
}

async fn reschedule(
    state: &AppState,
    id: i32,
    at: Instant,
    reset_attempts: bool,
) -> Result<(), sqlx::Error> {
    let sql = if reset_attempts {
        "UPDATE sms_flow_enrollments SET next_send_at = ?, attempt_count = 0 WHERE id = ? AND status = 'active'"
    } else {
        "UPDATE sms_flow_enrollments SET next_send_at = ? WHERE id = ? AND status = 'active'"
    };
    sqlx::query(sql)
        .bind(at.naive_utc())
        .bind(id)
        .execute(&state.pool)
        .await?;
    Ok(())
}

async fn inbound_sms_since(state: &AppState, due: &Due, phone: &str) -> Result<bool, sqlx::Error> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM (SELECT id FROM cloudtalk_sms WHERE company_id = ? AND direction = 'inbound' AND sender = ? AND created_date > ? LIMIT 1) ct
         UNION ALL
         SELECT id FROM (SELECT id FROM ringcentral_sms WHERE company_id = ? AND direction = 'inbound' AND sender = ? AND created_date > ? LIMIT 1) rc
         UNION ALL
         SELECT id FROM (SELECT id FROM telnyx_sms WHERE company_id = ? AND direction = 'inbound' AND phone_digits = RIGHT(?, 10) AND created_date > ? LIMIT 1) tx
         LIMIT 1",
    )
    .bind(due.company_id)
    .bind(phone)
    .bind(due.started_at)
    .bind(due.company_id)
    .bind(phone)
    .bind(due.started_at)
    .bind(due.company_id)
    .bind(phone)
    .bind(due.started_at)
    .fetch_optional(&state.pool)
    .await?;
    Ok(found.is_some())
}

async fn inbound_email_since(state: &AppState, due: &Due) -> Result<bool, sqlx::Error> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT e.id FROM emails e
           JOIN email_participants ep ON ep.email_id = e.id AND ep.type = 'from'
           LEFT JOIN customers_emails ce ON LOWER(TRIM(SUBSTRING_INDEX(SUBSTRING_INDEX(ce.email, '<', -1), '>', 1))) = ep.email
           LEFT JOIN customers c ON c.id = COALESCE(ep.customer_id, ce.customer_id) AND c.company_id = ? AND c.deleted_at IS NULL
          WHERE e.company_id = ? AND e.sender_user_id IS NULL AND e.deleted_at IS NULL AND e.sent_at > ?
            AND ((? IS NOT NULL AND c.id = ?)
              OR (CHAR_LENGTH(REGEXP_REPLACE(COALESCE(c.phone, ''), '[^0-9]', '')) >= 10
                  AND CAST(RIGHT(REGEXP_REPLACE(COALESCE(c.phone, ''), '[^0-9]', ''), 10) AS UNSIGNED) = CAST(? AS UNSIGNED))
              OR (CHAR_LENGTH(REGEXP_REPLACE(COALESCE(c.phone_2, ''), '[^0-9]', '')) >= 10
                  AND CAST(RIGHT(REGEXP_REPLACE(COALESCE(c.phone_2, ''), '[^0-9]', ''), 10) AS UNSIGNED) = CAST(? AS UNSIGNED)))
          LIMIT 1",
    )
    .bind(due.company_id)
    .bind(due.company_id)
    .bind(due.started_at)
    .bind(due.customer_id)
    .bind(due.customer_id)
    .bind(&due.phone_digits)
    .bind(&due.phone_digits)
    .fetch_optional(&state.pool)
    .await?;
    Ok(found.is_some())
}

/// The customer called in (`CloudTalk`) since the run started; a failed
/// lookup counts as no.
async fn incoming_call_since(state: &AppState, due: &Due) -> Result<bool, sqlx::Error> {
    use crate::routes::calls::to_e164;
    let mut phones: Vec<String> = due
        .phone_digits
        .as_deref()
        .and_then(to_e164)
        .into_iter()
        .collect();
    if let Some(customer_id) = due.customer_id {
        let row: Option<(Option<String>, Option<String>)> =
            sqlx::query_as("SELECT phone, phone_2 FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL")
                .bind(customer_id)
                .bind(due.company_id)
                .fetch_optional(&state.pool)
                .await?;
        for raw in row.into_iter().flat_map(<[_; 2]>::from).flatten() {
            if let Some(e164) = to_e164(&raw) {
                if !phones.contains(&e164) {
                    phones.push(e164);
                }
            }
        }
    }
    if phones.is_empty() {
        return Ok(false);
    }
    let from = due.started_at.format("%Y-%m-%d").to_string();
    let Ok(calls) =
        crate::routes::calls::cloudtalk_calls(state, due.company_id, &phones, Some(&from), None)
            .await
    else {
        return Ok(false);
    };
    let started = due.started_at.and_utc();
    Ok(calls.iter().any(|call| {
        call["Cdr"]["type"] == "incoming"
            && call["Cdr"]["started_at"]
                .as_str()
                .and_then(super::dates::js_date_parse)
                .and_then(super::dates::from_millis)
                .is_some_and(|at| at > started)
    }))
}

async fn customer_responded(state: &AppState, due: &Due) -> Result<bool, sqlx::Error> {
    if let Some(phone) = due.phone_digits.as_deref() {
        if inbound_sms_since(state, due, phone).await? {
            return Ok(true);
        }
    }
    if inbound_email_since(state, due).await? {
        return Ok(true);
    }
    incoming_call_since(state, due).await
}

#[derive(sqlx::FromRow, Debug, Clone)]
struct Step {
    position: i32,
    delay_minutes: i32,
    channel: String,
    custom_text: Option<String>,
    subject: Option<String>,
    template_body: Option<String>,
    template_subject: Option<String>,
}

impl Step {
    fn channel(&self) -> FollowupChannel {
        if self.channel == "email" {
            FollowupChannel::Email
        } else {
            FollowupChannel::Sms
        }
    }

    fn is_ai(&self) -> bool {
        self.custom_text.as_deref() == Some(SMS_FLOW_AI_SENTINEL)
    }
}

/// The current step and the next (the company guard turns a foreign
/// template into a missing one).
async fn current_and_next(state: &AppState, due: &Due) -> Result<Vec<Step>, sqlx::Error> {
    sqlx::query_as(
        "SELECT s.position, s.delay_minutes, COALESCE(CAST(s.channel AS CHAR), 'sms') AS channel, s.custom_text, s.subject,
                COALESCE(t.body, rt.body, et.template_body) AS template_body, et.template_subject
           FROM sms_flow_steps s
           LEFT JOIN cloudtalk_sms_templates t ON t.id = s.template_id AND t.company_id = ? AND COALESCE(s.channel, 'sms') = 'sms'
           LEFT JOIN ringcentral_sms_templates rt ON rt.id = s.template_id AND rt.company_id = ? AND COALESCE(s.channel, 'sms') = 'sms'
           LEFT JOIN email_templates et
             ON et.id = s.template_id AND et.company_id = ? AND COALESCE(s.channel, 'sms') = 'email' AND et.deleted_at IS NULL
          WHERE s.flow_id = ? AND s.position >= ?
          ORDER BY s.position ASC LIMIT 2",
    )
    .bind(due.company_id)
    .bind(due.company_id)
    .bind(due.company_id)
    .bind(due.flow_id)
    .bind(due.current_step_position)
    .fetch_all(&state.pool)
    .await
}

/// Delivered: the next step's time, or the run is complete. Paused rows
/// keep the bookkeeping too, so Resume never sends this step again.
async fn advance(
    state: &AppState,
    id: i32,
    next: Option<&Step>,
    now: Instant,
) -> Result<(), sqlx::Error> {
    let Some(next) = next else {
        sqlx::query(
            "UPDATE sms_flow_enrollments SET status = 'completed', next_send_at = NULL, anchor_at = UTC_TIMESTAMP(),
                    attempt_count = 0, error_message = NULL
              WHERE id = ? AND status IN ('active', 'paused')",
        )
        .bind(id)
        .execute(&state.pool)
        .await?;
        return Ok(());
    };
    let mut at = followup_next_send_at(now, f64::from(next.delay_minutes), next.channel());
    if next.is_ai() {
        at = next_send_at_avoiding_sunday(at, next.channel(), None);
    }
    sqlx::query(
        "UPDATE sms_flow_enrollments SET current_step_position = ?, anchor_at = ?, next_send_at = ?,
                attempt_count = 0, error_message = NULL
          WHERE id = ? AND status IN ('active', 'paused')",
    )
    .bind(next.position)
    .bind(now.naive_utc())
    .bind(at.naive_utc())
    .bind(id)
    .execute(&state.pool)
    .await?;
    Ok(())
}

/// A refused send: retried later (the claim keeps it out of the due window)
/// until the third attempt fails the run. Whether it became terminal.
async fn record_send_failure(
    state: &AppState,
    due: &Due,
    token: &str,
) -> Result<bool, sqlx::Error> {
    let attempt = due.attempt_count + 1;
    if attempt >= MAX_SEND_ATTEMPTS {
        let terminal = fail(state, due.id, token).await?;
        if terminal {
            tracing::error!(
                enrollment = due.id,
                flow = due.flow_id,
                token,
                "sms follow-up send failed terminally"
            );
        }
        return Ok(terminal);
    }
    sqlx::query("UPDATE sms_flow_enrollments SET attempt_count = ? WHERE id = ?")
        .bind(attempt)
        .bind(due.id)
        .execute(&state.pool)
        .await?;
    tracing::warn!(
        enrollment = due.id,
        flow = due.flow_id,
        attempt,
        token,
        "sms follow-up send failed, will retry"
    );
    Ok(false)
}

async fn set_customer(state: &AppState, id: i32, customer_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE sms_flow_enrollments SET customer_id = ? WHERE id = ?")
        .bind(customer_id)
        .bind(id)
        .execute(&state.pool)
        .await?;
    Ok(())
}

/// The run's text with `{{variables}}` filled; `None` when some stay unfilled.
async fn filled(
    state: &AppState,
    due: &Due,
    customer_id: Option<i32>,
    text: &str,
) -> Result<Option<String>, sqlx::Error> {
    use crate::routes::sms::flows::variable_names;
    if !text.contains("{{") {
        return Ok(Some(text.to_string()));
    }
    let data = common::crud::template::fetch_template_variable_data(
        &state.pool,
        due.user_id,
        None,
        customer_id,
        due.company_id,
    )
    .await?;
    let filled = common::utils::template::replace_template_variables(text, &data);
    Ok(variable_names(&filled)
        .iter()
        .all(String::is_empty)
        .then_some(filled))
}

enum Outcome {
    Sent,
    Failed,
    Stopped,
    Nothing,
}

/// Sends the step of one claimed enrollment (and the next at once when its
/// delay is zero). `Box::pin` for the zero-delay recursion.
fn process_one<'a>(
    state: &'a AppState,
    due: Due,
    run: &'a mut FollowupRun,
    clock: Option<Instant>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), sqlx::Error>> + Send + 'a>> {
    Box::pin(async move {
        if !claim(state, due.id).await? {
            return Ok(());
        }
        let outcome = Box::pin(send_step(state, &due, clock.unwrap_or_else(Utc::now))).await?;
        match outcome {
            Outcome::Sent => {
                run.sent += 1;
                // A zero-delay next step goes out in the same run.
                if let Some(next) =
                    due_enrollment(state, due.id, clock.unwrap_or_else(Utc::now)).await?
                {
                    process_one(state, next, run, clock).await?;
                }
            }
            Outcome::Failed => run.failed += 1,
            Outcome::Stopped => run.stopped += 1,
            Outcome::Nothing => {}
        }
        Ok(())
    })
}

#[allow(clippy::too_many_lines)]
async fn send_step(state: &AppState, due: &Due, now: Instant) -> Result<Outcome, sqlx::Error> {
    let mut request = FollowupRequest::new(due.company_id, due.user_id, FollowupChannel::Sms);
    request.customer_id = due.customer_id;
    request.phone_digits.clone_from(&due.phone_digits);
    request.customer_email.clone_from(&due.customer_email);
    let execution = gather_followup_context(&state.pool, &request, now).await?;
    let hold = execution.hold_until();
    if let Some(hold) = hold.filter(|_| must_reschedule_for_hold(now, hold)) {
        reschedule(state, due.id, hold, true).await?;
        return Ok(Outcome::Nothing);
    }
    // The customer asked for a phone call: no automated message before it.
    let pressure = &execution.contact_pressure;
    if pressure.manual_action_required == Some(ManualActionRequired::PhoneCall)
        && pressure.manual_action_status == ManualActionStatus::Pending
    {
        let recheck = (now + Duration::minutes(MANUAL_ACTION_RECHECK_MINUTES))
            .max(pressure.manual_action_due_at.unwrap_or(now));
        reschedule(
            state,
            due.id,
            clamp_followup_send_at(recheck, FollowupChannel::Sms),
            true,
        )
        .await?;
        return Ok(Outcome::Nothing);
    }
    if customer_responded(state, due).await? {
        let stopped = sqlx::query("UPDATE sms_flow_enrollments SET status = 'stopped_by_reply', next_send_at = NULL WHERE id = ? AND status = 'active'")
            .bind(due.id)
            .execute(&state.pool)
            .await?
            .rows_affected()
            == 1;
        return Ok(if stopped {
            Outcome::Stopped
        } else {
            Outcome::Nothing
        });
    }
    let steps = current_and_next(state, due).await?;
    let current = steps
        .iter()
        .find(|s| s.position == due.current_step_position);
    let next = steps
        .iter()
        .find(|s| s.position != due.current_step_position);
    let Some(current) =
        current.filter(|s| s.is_ai() || s.custom_text.is_some() || s.template_body.is_some())
    else {
        return Ok(if fail(state, due.id, "flow_step_invalid").await? {
            Outcome::Failed
        } else {
            Outcome::Nothing
        });
    };
    let channel = current.channel();
    if !is_followup_send_window_open(channel, now) {
        reschedule(state, due.id, clamp_followup_send_at(now, channel), false).await?;
        return Ok(Outcome::Nothing);
    }
    let provider = if channel == FollowupChannel::Sms {
        // The rep's provider: Telnyx when they have a Telnyx line there.
        let Some(provider) =
            crate::routes::sms::send::user_provider(&state.pool, due.company_id, due.user_id)
                .await?
        else {
            return Ok(
                if record_send_failure(state, due, "sms_not_configured").await? {
                    Outcome::Failed
                } else {
                    Outcome::Nothing
                },
            );
        };
        Some(provider)
    } else {
        None
    };

    let mut customer_id = due.customer_id;
    // A live call after the run started makes its planned copy stale.
    let replan = pressure.manual_action_status == ManualActionStatus::Completed
        && pressure
            .manual_action_completed_at
            .is_some_and(|at| at > due.started_at.and_utc());
    let (mut body, mut subject) = if current.is_ai() || replan {
        let mut write = request.clone();
        write.channel = channel;
        write.step_position = usize::try_from(current.position).unwrap_or(1);
        let gathered = gather_followup_context(&state.pool, &write, now).await?;
        if let (Some(found), None) = (gathered.customer_id, due.customer_id) {
            customer_id = Some(found);
            set_customer(state, due.id, found).await?;
        }
        let writer = Writer {
            pool: &state.pool,
            ai: &state.ai,
            company_id: due.company_id,
            user_id: due.user_id,
        };
        match complete_followup(writer, &write, &gathered).await {
            Ok(GeneratedFollowup::Skip { .. }) => {
                sqlx::query(
                    "UPDATE sms_flow_enrollments SET status = 'completed', next_send_at = NULL, anchor_at = UTC_TIMESTAMP(),
                            attempt_count = 0, error_message = NULL
                      WHERE id = ? AND status IN ('active', 'paused')",
                )
                .bind(due.id)
                .execute(&state.pool)
                .await?;
                return Ok(Outcome::Nothing);
            }
            Ok(GeneratedFollowup::Message { body, subject, .. }) => {
                (body, subject.unwrap_or_default())
            }
            Err(err) => {
                tracing::error!(%err, enrollment = due.id, "sms follow-up AI generation failed");
                return Ok(
                    if record_send_failure(state, due, "ai_generation_failed").await? {
                        Outcome::Failed
                    } else {
                        Outcome::Nothing
                    },
                );
            }
        }
    } else {
        (
            current
                .custom_text
                .clone()
                .or_else(|| current.template_body.clone())
                .unwrap_or_default(),
            current
                .subject
                .clone()
                .or_else(|| current.template_subject.clone())
                .unwrap_or_default(),
        )
    };
    if body.contains("{{") || subject.contains("{{") {
        if customer_id.is_none() {
            if let Some(phone) = due.phone_digits.as_deref() {
                // `fetchCustomerByPhone` of the CloudTalk service, whatever the provider.
                if let Some(customer) = crate::routes::sms::thread::customer_by_phone(
                    &state.pool,
                    SmsProvider::Cloudtalk,
                    due.company_id,
                    phone,
                )
                .await?
                {
                    customer_id = Some(customer.id);
                    set_customer(state, due.id, customer.id).await?;
                }
            }
        }
        match (
            filled(state, due, customer_id, &body).await?,
            filled(state, due, customer_id, &subject).await?,
        ) {
            (Some(b), Some(s)) => {
                body = b;
                subject = s;
            }
            _ => {
                return Ok(if fail(state, due.id, "unfilled_variables").await? {
                    Outcome::Failed
                } else {
                    Outcome::Nothing
                });
            }
        }
    }
    let Some(rep) = find_user(&state.pool, due.user_id, due.company_id).await? else {
        return Ok(if fail(state, due.id, "agent_not_linked").await? {
            Outcome::Failed
        } else {
            Outcome::Nothing
        });
    };
    let sent = match provider {
        None => send_email(state, due, &rep, customer_id, &subject, &body).await?,
        Some(company) => {
            let provider = rep_sms_provider(state, due.company_id, &rep)
                .await?
                .unwrap_or(company);
            send_sms(state, due, &rep, provider, current.position, &body).await?
        }
    };
    if let Err(outcome) = sent {
        return Ok(outcome);
    }
    // Delivered: never send this step again. A failed advance (twice) fails
    // the run instead.
    let advanced = match advance(state, due.id, next, now).await {
        Ok(()) => true,
        Err(first) => {
            tracing::error!(
                ?first,
                enrollment = due.id,
                "sms follow-up post-send advance failed, retrying"
            );
            advance(state, due.id, next, now).await.is_ok()
        }
    };
    if !advanced {
        return Ok(
            if fail(state, due.id, "advance_failed").await.unwrap_or(true) {
                Outcome::Failed
            } else {
                Outcome::Nothing
            },
        );
    }
    if next.is_some_and(|n| n.delay_minutes == 0) {
        sqlx::query(
            "UPDATE sms_flow_enrollments SET next_send_at = ? WHERE id = ? AND status = 'active'",
        )
        .bind(now.naive_utc())
        .bind(due.id)
        .execute(&state.pool)
        .await?;
    }
    Ok(Outcome::Sent)
}

/// `Name <a@b>` → `a@b` (`parseEmailAddress`).
fn bare_address(raw: &str) -> &str {
    let raw = raw.trim();
    raw.find('<')
        .and_then(|open| {
            raw[open + 1..]
                .find('>')
                .map(|close| &raw[open + 1..open + 1 + close])
        })
        .filter(|inner| !inner.is_empty())
        .map_or(raw, str::trim)
}

/// Sends an email step; `Err` with what to count when it did not go out.
async fn send_email(
    state: &AppState,
    due: &Due,
    rep: &SessionUser,
    customer_id: Option<i32>,
    subject: &str,
    body: &str,
) -> Result<Result<(), Outcome>, sqlx::Error> {
    let raw = due.customer_email.as_deref().map_or("", str::trim);
    if raw.is_empty() {
        return Ok(Err(if fail(state, due.id, "flow_step_invalid").await? {
            Outcome::Failed
        } else {
            Outcome::Nothing
        }));
    }
    let to = bare_address(raw).to_lowercase();
    if to.is_empty() {
        let terminal = record_send_failure(state, due, "invalid_email").await?;
        return Ok(Err(if terminal {
            Outcome::Failed
        } else {
            Outcome::Nothing
        }));
    }
    let subject = if subject.trim().is_empty() {
        "Follow up"
    } else {
        subject.trim()
    };
    let email = Cleaned {
        to: vec![to],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: subject.to_string(),
        body: body.to_string(),
        thread_id: None,
        deal_id: None,
        customer_id,
        send_together: false,
        attachments: Vec::new(),
        template_id: None,
        send_at: None,
        cancel_if_customer_messages: false,
        automated: true,
        signature_requests: Vec::new(),
        signature_send_key: None,
        delayed_email_id: None,
    };
    match deliver(state, rep, &email).await {
        Ok(_) => Ok(Ok(())),
        // Sent but not saved: the mail is out, never send it again.
        Err(crate::error::ApiError::Internal(reason)) => {
            tracing::error!(
                reason,
                enrollment = due.id,
                "follow-up email sent but not recorded"
            );
            Ok(Ok(()))
        }
        Err(err) => {
            tracing::warn!(?err, enrollment = due.id, "follow-up email failed");
            let terminal = record_send_failure(state, due, "send_failed").await?;
            Ok(Err(if terminal {
                Outcome::Failed
            } else {
                Outcome::Nothing
            }))
        }
    }
}

/// Sends a text step through the rep's provider.
async fn send_sms(
    state: &AppState,
    due: &Due,
    rep: &SessionUser,
    provider: SmsProvider,
    position: i32,
    body: &str,
) -> Result<Result<(), Outcome>, sqlx::Error> {
    let Some(seat) = crate::routes::sms::send::seat(&state.pool, provider, rep).await? else {
        return Ok(Err(if fail(state, due.id, "agent_not_linked").await? {
            Outcome::Failed
        } else {
            Outcome::Nothing
        }));
    };
    let Some(phone) = due
        .phone_digits
        .as_deref()
        .filter(|p| crate::routes::calls::to_e164(p).is_some())
    else {
        // Short codes start a run but cannot be texted.
        return Ok(Err(if fail(state, due.id, "invalid_phone").await? {
            Outcome::Failed
        } else {
            Outcome::Nothing
        }));
    };
    let outgoing = OutgoingSms {
        provider: provider.as_str().to_string(),
        company_id: due.company_id,
        user_id: rep.id,
        agent_id: seat.agent_id.clone(),
        phone_digits: phone.to_string(),
        text: body.trim().to_string(),
        attachments: Vec::new(),
        is_automated: true,
        idempotency_key: Some(format!("sms-flow-{}-{}", due.id, position)),
    };
    match crate::routes::sms::send::send_text(state, provider, &seat, &outgoing).await {
        Ok(_) => Ok(Ok(())),
        // No number for the agent, or a phone that texted STOP, cannot fix
        // itself.
        Err(failure) if matches!(failure.error.as_str(), "agent_no_number" | "opted_out") => {
            Ok(Err(if fail(state, due.id, &failure.error).await? {
                Outcome::Failed
            } else {
                Outcome::Nothing
            }))
        }
        Err(failure) => {
            let token = if failure.error.is_empty() {
                "send_failed".to_string()
            } else {
                failure.error
            };
            let terminal = record_send_failure(state, due, &token).await?;
            Ok(Err(if terminal {
                Outcome::Failed
            } else {
                Outcome::Nothing
            }))
        }
    }
}

/// The provider the company texts through (`getCompanySmsProvider`).
pub async fn company_sms_provider(
    state: &AppState,
    company_id: i32,
) -> Result<Option<SmsProvider>, sqlx::Error> {
    let features =
        crate::routes::company::queries::company_features(&state.pool, company_id).await?;
    Ok(features.sms_provider())
}

/// The provider `rep` texts through, as their inbox picks it
/// (`prefersRingcentralInbox`): a linked `RingCentral` line wins, then a
/// `CloudTalk` agent, then `RingCentral` when both are connected. Telnyx
/// comes before them when the company texts through it and `rep` has a
/// line there, and after them when the company texts only through it.
pub async fn rep_sms_provider(
    state: &AppState,
    company_id: i32,
    rep: &SessionUser,
) -> Result<Option<SmsProvider>, sqlx::Error> {
    let features =
        crate::routes::company::queries::company_features(&state.pool, company_id).await?;
    if features.telnyx_sms
        && crate::phone::texting::user_line(&state.pool, company_id, rep.id)
            .await?
            .is_some()
    {
        return Ok(Some(SmsProvider::Telnyx));
    }
    Ok(rep_provider(features.cloudtalk, features.ringcentral, rep)
        .or_else(|| features.telnyx_sms.then_some(SmsProvider::Telnyx)))
}

fn rep_provider(cloudtalk: bool, ringcentral: bool, rep: &SessionUser) -> Option<SmsProvider> {
    if !ringcentral {
        return cloudtalk.then_some(SmsProvider::Cloudtalk);
    }
    let ringcentral_line = SmsProvider::Ringcentral.agent_of(rep).is_some()
        || rep
            .ringcentral_phone_number
            .as_deref()
            .is_some_and(|phone| !phone.trim().is_empty());
    if cloudtalk && !ringcentral_line && SmsProvider::Cloudtalk.agent_of(rep).is_some() {
        Some(SmsProvider::Cloudtalk)
    } else {
        Some(SmsProvider::Ringcentral)
    }
}

/// Sends the step of one due enrollment now (a rep started a flow or the
/// banner's countdown ran out).
pub async fn process_due_enrollment(state: &AppState, enrollment_id: i32) -> FollowupRun {
    process_due_enrollment_at(state, enrollment_id, None).await
}

/// [`process_due_enrollment`] with a fixed clock (`None`: now).
pub async fn process_due_enrollment_at(
    state: &AppState,
    enrollment_id: i32,
    clock: Option<Instant>,
) -> FollowupRun {
    let mut run = FollowupRun::default();
    match due_enrollment(state, enrollment_id, clock.unwrap_or_else(Utc::now)).await {
        Ok(Some(due)) => {
            run.processed = 1;
            if let Err(err) = process_one(state, due, &mut run, clock).await {
                tracing::error!(
                    ?err,
                    enrollment_id,
                    "sms follow-up immediate dispatch failed"
                );
            }
        }
        Ok(None) => {}
        Err(err) => tracing::error!(?err, enrollment_id, "sms follow-up lookup failed"),
    }
    run
}

/// Sends every due step (at most 50, 10 per company).
pub async fn process_due_followups(state: &AppState) -> Result<FollowupRun, sqlx::Error> {
    process_due_followups_at(state, None).await
}

/// [`process_due_followups`] with a fixed clock (`None`: now).
pub async fn process_due_followups_at(
    state: &AppState,
    clock: Option<Instant>,
) -> Result<FollowupRun, sqlx::Error> {
    let due = due_enrollments(state, clock.unwrap_or_else(Utc::now)).await?;
    let mut run = FollowupRun {
        processed: due.len(),
        ..FollowupRun::default()
    };
    for enrollment in due {
        let id = enrollment.id;
        if let Err(err) = process_one(state, enrollment, &mut run, clock).await {
            tracing::error!(?err, enrollment = id, "sms follow-up dispatch failed");
        }
    }
    Ok(run)
}
