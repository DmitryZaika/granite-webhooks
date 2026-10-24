//! A customer's run through an SMS / email flow (`sms_flow_enrollments`).
//!
//! Starting, switching, pausing, resuming, stopping and sending the next
//! step early, and what the conversation banner shows. At most one active or
//! paused enrollment per customer and phone. Port of
//! `smsFlowEnrollments.server.ts`.

use super::send_window::{
    FollowupChannel, followup_next_send_at, should_send_followup_immediately,
};
use crate::auth::session::SessionUser;
use crate::routes::sms::flows::{StepChannel, steps_by_flow};
use crate::routes::sms::provider::SmsProvider;
use crate::routes::sms::schemas::ActiveFlow;
use chrono::{Duration, NaiveDateTime, Utc};
use sqlx::{MySql, MySqlPool, Transaction};

/// Whose enrollment: the SMS thread's phone or the customer.
#[derive(Debug, Clone)]
pub enum Target {
    /// Last ten digits (shorter numbers as they are).
    Phone(String),
    Customer(i32),
}

/// Why an action was refused (the CRM's error tokens).
pub type Refusal = &'static str;

/// The ten-digit form of a phone (`canonicalPhone10`).
pub fn canonical_phone10(raw: &str) -> String {
    let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
    if digits.len() > 10 {
        digits[digits.len() - 10..].to_string()
    } else {
        digits
    }
}

#[derive(sqlx::FromRow)]
struct SummaryRow {
    enrollment_id: i32,
    flow_id: i32,
    flow_name: String,
    status: String,
    next_step_position: i32,
    next_step_name: Option<String>,
    next_step_custom_text: Option<String>,
    next_step_template_body: Option<String>,
    next_send_at: Option<NaiveDateTime>,
    error_message: Option<String>,
    total_steps: i64,
}

fn step_preview(custom: Option<&str>, template: Option<&str>) -> Option<String> {
    if let Some(custom) = custom.filter(|c| *c != super::classify::SMS_FLOW_AI_SENTINEL) {
        return Some(custom.trim().to_string()).filter(|s| !s.is_empty());
    }
    template
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The customer's flow the banner shows (active or paused, else the last
/// failed one). Anyone who can open the customer sees it, like the CRM.
pub async fn summary_for_customer(
    pool: &MySqlPool,
    user: &SessionUser,
    customer_id: i32,
) -> Result<Option<ActiveFlow>, sqlx::Error> {
    let row: Option<SummaryRow> = sqlx::query_as(
        "SELECT e.id AS enrollment_id, e.flow_id, f.name AS flow_name, CAST(e.status AS CHAR) AS status,
                e.current_step_position AS next_step_position,
                COALESCE(t.name, rt.name, et.template_name) AS next_step_name,
                st.custom_text AS next_step_custom_text,
                COALESCE(t.body, rt.body, et.template_body) AS next_step_template_body,
                e.next_send_at, e.error_message,
                (SELECT COUNT(*) FROM sms_flow_steps s2 WHERE s2.flow_id = e.flow_id) AS total_steps
           FROM sms_flow_enrollments e
           JOIN sms_flows f ON f.id = e.flow_id
           LEFT JOIN sms_flow_steps st ON st.flow_id = e.flow_id AND st.position = e.current_step_position
           LEFT JOIN cloudtalk_sms_templates t
             ON t.id = st.template_id AND t.company_id = e.company_id AND COALESCE(st.channel, 'sms') = 'sms'
           LEFT JOIN ringcentral_sms_templates rt
             ON rt.id = st.template_id AND rt.company_id = e.company_id AND COALESCE(st.channel, 'sms') = 'sms'
           LEFT JOIN email_templates et
             ON et.id = st.template_id AND et.company_id = e.company_id
            AND COALESCE(st.channel, 'sms') = 'email' AND et.deleted_at IS NULL
          WHERE e.company_id = ? AND e.customer_id = ?
            AND e.status IN ('active', 'paused', 'failed')
          ORDER BY CASE WHEN e.status IN ('active', 'paused') THEN 0 ELSE 1 END, e.updated_at DESC
          LIMIT 1",
    )
    .bind(user.company_id)
    .bind(customer_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| ActiveFlow {
        next_step_preview: step_preview(
            row.next_step_custom_text.as_deref(),
            row.next_step_template_body.as_deref(),
        ),
        enrollment_id: row.enrollment_id,
        flow_id: row.flow_id,
        flow_name: row.flow_name,
        status: row.status,
        next_step_position: row.next_step_position,
        next_step_name: row.next_step_name,
        next_send_at: row.next_send_at.map(|at| at.and_utc()),
        total_steps: row.total_steps,
        error_message: row.error_message,
    }))
}

/// The thread's flow the banner shows, for someone who may see it (see
/// [`crate::routes::sms::thread::active_flow`]).
pub async fn summary_for_phone(
    pool: &MySqlPool,
    provider: SmsProvider,
    user: &SessionUser,
    phone10: &str,
) -> Result<Option<ActiveFlow>, sqlx::Error> {
    use crate::routes::sms::scope::{SmsScope, thread_access};
    let scope = SmsScope::All.for_user(user);
    let sees_thread = thread_access(pool, provider, user, scope, None, phone10)
        .await?
        .is_some();
    crate::routes::sms::thread::active_flow(pool, user, phone10, sees_thread).await
}

#[derive(sqlx::FromRow, Clone, Copy)]
struct Controlled {
    id: i32,
    user_id: i32,
    customer_id: Option<i32>,
    active: bool,
}

async fn active_or_paused(
    pool: &MySqlPool,
    company_id: i32,
    target: &Target,
) -> Result<Option<Controlled>, sqlx::Error> {
    let (column, value) = match target {
        Target::Phone(phone) => ("customer_phone_digits", phone.clone()),
        Target::Customer(id) => ("customer_id", id.to_string()),
    };
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, user_id, customer_id, status = 'active' AS active FROM sms_flow_enrollments
          WHERE company_id = ? AND {column} = ? AND status IN ('active', 'paused') LIMIT 1"
    )))
    .bind(company_id)
    .bind(value)
    .fetch_optional(pool)
    .await
}

/// Admins, the rep who started it, and the customer's sales rep.
async fn can_control(
    pool: &MySqlPool,
    user: &SessionUser,
    user_id: i32,
    customer_id: Option<i32>,
) -> Result<bool, sqlx::Error> {
    if user.is_admin || user.is_superuser || user_id == user.id {
        return Ok(true);
    }
    let Some(customer_id) = customer_id else {
        return Ok(false);
    };
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM customers WHERE id = ? AND company_id = ? AND sales_rep = ?",
    )
    .bind(customer_id)
    .bind(user.company_id)
    .bind(user.id)
    .fetch_optional(pool)
    .await?;
    Ok(found.is_some())
}

/// What a control action does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Pause,
    Resume,
    Stop,
    /// The next step is due now (the AI follow-up "Send now").
    SendNow,
}

/// Pauses, resumes, stops or advances the target's enrollment. `Ok(true)`
/// when the next step should be sent right away.
pub async fn control(
    pool: &MySqlPool,
    user: &SessionUser,
    target: &Target,
    action: Control,
) -> Result<Result<bool, Refusal>, sqlx::Error> {
    let Some(existing) = active_or_paused(pool, user.company_id, target).await? else {
        return Ok(Err("enrollment_not_found"));
    };
    if !can_control(pool, user, existing.user_id, existing.customer_id).await? {
        return Ok(Err("forbidden"));
    }
    let (sql, needs_active, needs_paused, missing) = match action {
        Control::Pause => (
            "UPDATE sms_flow_enrollments SET status = 'paused', updated_at = UTC_TIMESTAMP() WHERE id = ? AND status = 'active'",
            true,
            false,
            "invalid_status",
        ),
        Control::Resume => (
            "UPDATE sms_flow_enrollments SET status = 'active', updated_at = UTC_TIMESTAMP() WHERE id = ? AND status = 'paused'",
            false,
            true,
            "invalid_status",
        ),
        Control::Stop => (
            "UPDATE sms_flow_enrollments SET status = 'cancelled', updated_at = UTC_TIMESTAMP()
              WHERE id = ? AND status IN ('active', 'paused')",
            false,
            false,
            "enrollment_not_found",
        ),
        Control::SendNow => (
            "UPDATE sms_flow_enrollments SET next_send_at = UTC_TIMESTAMP(), updated_at = UTC_TIMESTAMP()
              WHERE id = ? AND status = 'active'",
            true,
            false,
            "invalid_status",
        ),
    };
    if (needs_active && !existing.active) || (needs_paused && existing.active) {
        return Ok(Err("invalid_status"));
    }
    let changed = sqlx::query(sql)
        .bind(existing.id)
        .execute(pool)
        .await?
        .rows_affected()
        > 0;
    // A change between the read and the write loses.
    Ok(if changed {
        Ok(action == Control::SendNow)
    } else {
        Err(missing)
    })
}

/// The banner's countdown reached zero: the step is due (within 5 s of
/// clock skew) and may be sent now.
pub fn dispatchable(flow: Option<&ActiveFlow>) -> Result<(), Refusal> {
    let Some(flow) = flow else {
        return Err("enrollment_not_found");
    };
    if flow.status != "active" {
        return Err("invalid_status");
    }
    if flow
        .next_send_at
        .is_some_and(|at| at > Utc::now() + Duration::seconds(5))
    {
        return Err("not_due");
    }
    Ok(())
}

/// What a start needs, checked.
struct Prepared {
    flow_id: i32,
    phone: Option<String>,
    email: Option<String>,
    customer_id: Option<i32>,
    first_delay: f64,
    first_channel: FollowupChannel,
}

/// The customer's ten-digit phone and email (`None` when not the company's).
pub(crate) async fn customer_contact(
    pool: &MySqlPool,
    company_id: i32,
    customer_id: i32,
) -> Result<Option<(Option<String>, Option<String>)>, sqlx::Error> {
    let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT c.phone, COALESCE(primary_email.email, any_email.email) AS email
           FROM customers c
           LEFT JOIN customers_emails primary_email ON primary_email.id = c.email_id AND primary_email.customer_id = c.id
           LEFT JOIN customers_emails any_email ON any_email.customer_id = c.id
          WHERE c.id = ? AND c.company_id = ? AND c.deleted_at IS NULL
          LIMIT 1",
    )
    .bind(customer_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(phone, email)| {
        let digits = canonical_phone10(phone.as_deref().unwrap_or(""));
        (
            (digits.len() == 10).then_some(digits),
            email
                .map(|e| e.trim().to_lowercase())
                .filter(|e| !e.is_empty()),
        )
    }))
}

async fn prepare(
    pool: &MySqlPool,
    user: &SessionUser,
    sms_provider: Option<SmsProvider>,
    flow_id: i32,
    phone: Option<&str>,
    customer_id: Option<i32>,
) -> Result<Result<Prepared, Refusal>, sqlx::Error> {
    let flow: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM sms_flows WHERE id = ? AND company_id = ? AND deleted_at IS NULL AND (user_id IS NULL OR user_id = ?)",
    )
    .bind(flow_id)
    .bind(user.company_id)
    .bind(user.id)
    .fetch_optional(pool)
    .await?;
    if flow.is_none() {
        return Ok(Err("flow_not_found"));
    }
    let steps = steps_by_flow(pool, user.company_id, &[flow_id])
        .await?
        .remove(&flow_id)
        .unwrap_or_default();
    let Some(first) = steps.first() else {
        return Ok(Err("flow_has_no_steps"));
    };
    let has_sms = steps.iter().any(|s| s.channel == StepChannel::Sms);
    let has_email = steps.iter().any(|s| s.channel == StepChannel::Email);
    let mut customer_id = customer_id;
    let mut phone = phone.map(canonical_phone10).filter(|p| p.len() >= 5);
    let mut email = None;
    if let Some(id) = customer_id {
        let Some((customer_phone, customer_email)) =
            customer_contact(pool, user.company_id, id).await?
        else {
            return Ok(Err("customer_not_found"));
        };
        phone = phone.or(customer_phone);
        email = customer_email;
    } else if let Some(digits) = &phone {
        let provider = sms_provider.unwrap_or(SmsProvider::Cloudtalk);
        customer_id =
            crate::routes::sms::thread::customer_by_phone(pool, provider, user.company_id, digits)
                .await?
                .map(|c| c.id);
        if let Some(id) = customer_id {
            email = customer_contact(pool, user.company_id, id)
                .await?
                .and_then(|(_, e)| e);
        }
    }
    if has_sms {
        // SMS follow-ups only for a rep with their own CloudTalk or
        // RingCentral number.
        let filled =
            |value: &Option<String>| value.as_deref().is_some_and(|v| !v.trim().is_empty());
        if !filled(&user.cloudtalk_phone_number) && !filled(&user.ringcentral_phone_number) {
            return Ok(Err("agent_not_linked"));
        }
        // The rep's line: the user needs an agent (a Telnyx line) there.
        let seated = match sms_provider {
            Some(provider) => crate::routes::sms::send::seat(pool, provider, user)
                .await?
                .is_some(),
            None => false,
        };
        if !seated {
            return Ok(Err("agent_not_linked"));
        }
        if phone.is_none() {
            return Ok(Err("invalid_phone"));
        }
    }
    if has_email && email.is_none() {
        return Ok(Err("missing_email"));
    }
    if !has_sms && !has_email {
        return Ok(Err("flow_has_no_steps"));
    }
    if phone.is_none() && email.is_none() {
        return Ok(Err("invalid_phone"));
    }
    Ok(Ok(Prepared {
        flow_id,
        phone,
        email,
        customer_id,
        first_delay: f64::from(first.delay_minutes),
        first_channel: if first.channel == StepChannel::Email {
            FollowupChannel::Email
        } else {
            FollowupChannel::Sms
        },
    }))
}

fn target_clause(phone: Option<&str>, customer_id: Option<i32>) -> Option<(String, Vec<String>)> {
    let mut clauses = Vec::new();
    let mut values = Vec::new();
    if let Some(id) = customer_id {
        clauses.push("customer_id = ?");
        values.push(id.to_string());
    }
    if let Some(phone) = phone {
        clauses.push("customer_phone_digits = ?");
        values.push(phone.to_string());
    }
    (!clauses.is_empty()).then(|| (clauses.join(" OR "), values))
}

async fn locked_running(
    tx: &mut Transaction<'_, MySql>,
    company_id: i32,
    phone: Option<&str>,
    customer_id: Option<i32>,
) -> Result<Vec<Controlled>, sqlx::Error> {
    let Some((clause, values)) = target_clause(phone, customer_id) else {
        return Ok(Vec::new());
    };
    let mut query = sqlx::query_as::<_, Controlled>(sqlx::AssertSqlSafe(format!(
        "SELECT id, user_id, customer_id, status = 'active' AS active FROM sms_flow_enrollments
          WHERE company_id = ? AND status IN ('active', 'paused') AND ({clause}) FOR UPDATE"
    )))
    .bind(company_id);
    for value in values {
        query = query.bind(value);
    }
    query.fetch_all(&mut **tx).await
}

/// Starts the flow for the customer or phone (`switch`: cancels the running
/// one first, in the same transaction). `Ok(true)` when the first step
/// should go out right away.
pub async fn start(
    pool: &MySqlPool,
    user: &SessionUser,
    sms_provider: Option<SmsProvider>,
    flow_id: i32,
    phone: Option<&str>,
    customer_id: Option<i32>,
    switch: bool,
) -> Result<Result<bool, Refusal>, sqlx::Error> {
    let prepared = match prepare(pool, user, sms_provider, flow_id, phone, customer_id).await? {
        Ok(prepared) => prepared,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let mut tx = pool.begin().await?;
    let running = locked_running(
        &mut tx,
        user.company_id,
        prepared.phone.as_deref(),
        prepared.customer_id,
    )
    .await?;
    if !switch && !running.is_empty() {
        tx.rollback().await?;
        return Ok(Err("flow_already_active"));
    }
    if switch {
        if let Some(existing) = running.first() {
            if !can_control(pool, user, existing.user_id, existing.customer_id).await? {
                tx.rollback().await?;
                return Ok(Err("forbidden"));
            }
        }
        if let Some((clause, values)) =
            target_clause(prepared.phone.as_deref(), prepared.customer_id)
        {
            let mut stop = sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE sms_flow_enrollments SET status = 'cancelled', updated_at = UTC_TIMESTAMP()
                  WHERE company_id = ? AND status IN ('active', 'paused') AND ({clause})"
            )))
            .bind(user.company_id);
            for value in values {
                stop = stop.bind(value);
            }
            stop.execute(&mut *tx).await?;
        }
    }
    let next = followup_next_send_at(Utc::now(), prepared.first_delay, prepared.first_channel);
    sqlx::query(
        "INSERT INTO sms_flow_enrollments
           (flow_id, company_id, customer_phone_digits, customer_email, customer_id, user_id,
            status, current_step_position, anchor_at, next_send_at)
         VALUES (?, ?, ?, ?, ?, ?, 'active', 1, UTC_TIMESTAMP(), ?)",
    )
    .bind(prepared.flow_id)
    .bind(user.company_id)
    .bind(&prepared.phone)
    .bind(&prepared.email)
    .bind(prepared.customer_id)
    .bind(user.id)
    .bind(next.naive_utc())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let immediately =
        should_send_followup_immediately(prepared.first_delay, prepared.first_channel, Utc::now());
    Ok(Ok(immediately))
}

/// The customer wrote or talked to the company: their running flows stop
/// (by this phone, the customer and the customer's other phone). How many.
pub async fn stop_after_customer_conversation(
    pool: &MySqlPool,
    provider: SmsProvider,
    company_id: i32,
    phone: &str,
) -> Result<u64, sqlx::Error> {
    let digits = canonical_phone10(phone);
    if digits.is_empty() {
        return Ok(0);
    }
    let mut affected = stop_phone_runs(pool, company_id, &digits).await?;
    let Some(customer) =
        crate::routes::sms::thread::customer_by_phone(pool, provider, company_id, &digits).await?
    else {
        return Ok(affected);
    };
    affected += stop_customer_runs(pool, company_id, customer.id, &digits).await?;
    Ok(affected)
}

/// The customer of a Granite Phone call talked to the company (or called
/// it): their running flows stop, like [`stop_after_customer_conversation`]
/// (by the call's number, the customer and the customer's other phone).
/// The call names its customer, so no SMS provider is needed: a company
/// that texts through none stops its email flows too. How many.
pub async fn stop_after_call(
    pool: &MySqlPool,
    company_id: i32,
    phone: &str,
    customer_id: i32,
) -> Result<u64, sqlx::Error> {
    let digits = canonical_phone10(phone);
    let mut affected = if digits.is_empty() {
        0
    } else {
        stop_phone_runs(pool, company_id, &digits).await?
    };
    affected += stop_customer_runs(pool, company_id, customer_id, &digits).await?;
    Ok(affected)
}

/// Stops the running flows of the phone `digits` (last 10): how many.
async fn stop_phone_runs(
    pool: &MySqlPool,
    company_id: i32,
    digits: &str,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(
        "UPDATE sms_flow_enrollments SET status = 'stopped_by_reply', updated_at = UTC_TIMESTAMP()
          WHERE company_id = ? AND customer_phone_digits = ? AND status IN ('active', 'paused')",
    )
    .bind(company_id)
    .bind(digits)
    .execute(pool)
    .await?
    .rows_affected())
}

/// Stops the running flows of the customer and of their phones other than
/// `digits` (stopped already): how many.
async fn stop_customer_runs(
    pool: &MySqlPool,
    company_id: i32,
    customer_id: i32,
    digits: &str,
) -> Result<u64, sqlx::Error> {
    let mut affected = sqlx::query(
        "UPDATE sms_flow_enrollments SET status = 'stopped_by_reply', updated_at = UTC_TIMESTAMP()
          WHERE company_id = ? AND customer_id = ? AND status IN ('active', 'paused')",
    )
    .bind(company_id)
    .bind(customer_id)
    .execute(pool)
    .await?
    .rows_affected();
    let phones: Option<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT phone, phone_2 FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL")
            .bind(customer_id)
            .bind(company_id)
            .fetch_optional(pool)
            .await?;
    if let Some((phone, phone_2)) = phones {
        for raw in <[_; 2]>::from((phone, phone_2)).into_iter().flatten() {
            let other = canonical_phone10(&raw);
            if !other.is_empty() && other != digits {
                affected += stop_phone_runs(pool, company_id, &other).await?;
            }
        }
    }
    Ok(affected)
}
