//! What a call's outcome sets off: our pushes to the people it rang (missed
//! call, voicemail, AI message), and the CRM follow-through when a call with
//! a customer ends:
//!
//! - the customer calling us (any outcome), or an answered call of 90
//!   seconds or more that we placed, moves the call's deal (the customer's
//!   oldest open deal) out of the first stage of its pipeline to the next,
//!   like the CloudTalk and RingCentral call webhooks do (a won deal stays);
//!   the stage history and drips follow
//!   (`routes::deals::service::transition_stage`);
//! - on the same calls the customer's flows stop (their number, the
//!   customer and the customer's other number), whatever the company texts
//!   through (`followups::enrollments::stop_after_call`).
//!
//! Shorter answered calls we placed (30 to 90 seconds) are checked once
//! their transcript is written (the phone job's AI notes).
//!
//! All of it is best effort: a failure is logged (ids only) and never fails
//! Telnyx's callback.

use super::call_log::{CallFacts, Ended};
use super::calls::{CallDirection, CallStatus};
use super::company;
use super::devices::push_to_users;
use super::routing;
use crate::followups::enrollments::stop_after_call;
use crate::routes::deals::service::transition_stage;
use crate::state::AppState;
use chrono::Utc;
use lambda_http::tracing;
use sqlx::MySqlPool;
use std::collections::BTreeMap;

/// A talk this long (seconds) counts as having reached the customer.
pub const CONVERSATION_SECS: i32 = 90;

/// Our pushes about a call (FCM data `type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Push {
    /// Nobody answered.
    MissedCall,
    /// The caller left a voicemail.
    Voicemail,
    /// The AI Assistant took a message.
    AiMessage,
}

impl Push {
    const fn as_str(self) -> &'static str {
        match self {
            Self::MissedCall => "missed_call",
            Self::Voicemail => "voicemail",
            Self::AiMessage => "ai_message",
        }
    }
}

/// Who hears about an incoming call: its user when someone took it, else
/// the users its number rings now.
async fn recipients(state: &AppState, call: &CallFacts) -> sqlx::Result<Vec<i32>> {
    if let Some(user_id) = call.user_id {
        return Ok(vec![user_id]);
    }
    let Some(number_id) = call.number_id else {
        return Ok(Vec::new());
    };
    let pool = &state.pool;
    let (Some(config), Some(line)) = (
        company::load(pool, call.company_id).await?,
        routing::line(pool, call.company_id, number_id).await?,
    ) else {
        return Ok(Vec::new());
    };
    Ok(routing::ringers(pool, &config, &line, Utc::now())
        .await?
        .into_iter()
        .map(|ringer| ringer.user_id)
        .collect())
}

/// Pushes `kind` about an incoming call (`{granite: "1", type, callId}`) to
/// the phones of the people it is for; never for test calls or outgoing
/// calls.
pub async fn notify(state: &AppState, call: &CallFacts, kind: Push) {
    if call.direction != CallDirection::Inbound || call.is_test() {
        return;
    }
    let users = match recipients(state, call).await {
        Ok(users) => users,
        Err(err) => {
            tracing::warn!(call_id = call.id, error = %err, "push recipients not read");
            return;
        }
    };
    let data = BTreeMap::from([
        ("granite".to_string(), "1".to_string()),
        ("type".to_string(), kind.as_str().to_string()),
        ("callId".to_string(), call.id.to_string()),
    ]);
    if let Err(err) = push_to_users(state, call.company_id, &users, &data).await {
        tracing::warn!(call_id = call.id, error = %err, "call push not sent");
    }
}

/// What follows a call that just ended: a missed-call push for an incoming
/// call the caller hung up on while it rang, and the CRM follow-through for
/// a call with a customer (see the module docs).
pub async fn after_end(state: &AppState, ended: &Ended) {
    let call = &ended.before;
    if call.is_test() {
        return;
    }
    if call.direction == CallDirection::Inbound
        && call.status == CallStatus::Ringing
        && ended.status == CallStatus::Canceled
    {
        notify(state, call, Push::MissedCall).await;
    }
    let Some(customer_id) = call.customer_id else {
        return;
    };
    let talked = ended.status == CallStatus::Answered
        && ended
            .talk_secs
            .is_some_and(|secs| secs >= CONVERSATION_SECS);
    // CloudTalk and RingCentral count any call from the customer.
    if talked || call.direction == CallDirection::Inbound {
        reached(state, call, customer_id).await;
    }
}

/// The customer reached us on `call` (or talked on a call we placed): the
/// call's deal leaves its first stage, and their flows stop.
pub async fn reached(state: &AppState, call: &CallFacts, customer_id: i32) {
    if let Some(deal_id) = call.deal_id {
        match move_out_of_first_stage(&state.pool, call.company_id, deal_id).await {
            Ok(true) => tracing::info!(
                call_id = call.id,
                deal_id,
                "deal left its first stage after a call"
            ),
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(call_id = call.id, deal_id, error = %err, "deal not moved after a call")
            }
        }
    }
    stop_flows(state, call, customer_id).await;
}

/// Stops the customer's flows (they reached us), whatever the company texts
/// through.
async fn stop_flows(state: &AppState, call: &CallFacts, customer_id: i32) {
    match stop_after_call(
        &state.pool,
        call.company_id,
        &call.other_number,
        customer_id,
    )
    .await
    {
        Ok(0) => {}
        Ok(stopped) => tracing::info!(
            call_id = call.id,
            customer_id,
            stopped,
            "SMS flows stopped after a call"
        ),
        Err(err) => {
            tracing::warn!(call_id = call.id, error = %err, "SMS flows not stopped after a call")
        }
    }
}

/// Moves the company's deal from the first stage of its pipeline to the
/// next one (the CloudTalk webhook's rule: a won deal stays, a deal in any
/// other stage stays): whether it moved.
pub async fn move_out_of_first_stage(
    pool: &MySqlPool,
    company_id: i32,
    deal_id: i64,
) -> sqlx::Result<bool> {
    let deal: Option<(i32, i32, i32, Option<i32>)> = sqlx::query_as(
        "SELECT d.list_id, dl.group_id, dl.position, d.is_won
           FROM deals d
           JOIN customers c ON c.id = d.customer_id AND c.company_id = ? AND c.deleted_at IS NULL
           JOIN deals_list dl ON dl.id = d.list_id AND dl.deleted_at IS NULL
          WHERE d.id = ? AND d.deleted_at IS NULL",
    )
    .bind(company_id)
    .bind(deal_id)
    .fetch_optional(pool)
    .await?;
    let Some((list_id, group_id, position, is_won)) = deal else {
        return Ok(false);
    };
    if is_won == Some(1) {
        return Ok(false);
    }
    let first: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM deals_list WHERE group_id = ? AND deleted_at IS NULL
          ORDER BY position, id LIMIT 1",
    )
    .bind(group_id)
    .fetch_optional(pool)
    .await?;
    if first != Some(list_id) {
        return Ok(false);
    }
    let next: Option<(i32, String)> = sqlx::query_as(
        "SELECT id, name FROM deals_list
          WHERE group_id = ? AND deleted_at IS NULL AND (position > ? OR (position = ? AND id > ?))
          ORDER BY position, id LIMIT 1",
    )
    .bind(group_id)
    .bind(position)
    .bind(position)
    .bind(list_id)
    .fetch_optional(pool)
    .await?;
    let Some((next_id, next_name)) = next else {
        return Ok(false);
    };
    let moved = sqlx::query(
        "UPDATE deals SET list_id = ?, status = ?, position = 0, is_won = NULL, lost_reason = NULL,
                          updated_at = NOW()
          WHERE id = ? AND list_id = ? AND deleted_at IS NULL",
    )
    .bind(next_id)
    .bind(&next_name)
    .bind(deal_id)
    .bind(list_id)
    .execute(pool)
    .await?
    .rows_affected();
    if moved == 0 {
        return Ok(false);
    }
    transition_stage(pool, deal_id, next_id).await?;
    Ok(true)
}
