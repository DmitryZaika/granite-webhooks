//! `POST /v1/jobs/contact-sync`: every customer into the company's
//! `RingCentral` and `CloudTalk` contacts.
//!
//! Edits already sync right away; this catches customers created elsewhere
//! (lead forms, imports, inbound texts) and syncs that failed. Each run takes
//! a few customers without a contact per provider; one that is still missing
//! after its try waits a while (`Provider::retry_after_hours`) for the next.
//!
//! It also moves `RingCentral` SMS subscriptions to the current webhooks
//! token: a few companies a run whose subscription was given another token
//! (or one not known) are renewed with this one, so changing `WEBHOOKS_KEY`
//! reaches every subscription without anyone reconnecting. A renewal never
//! creates a subscription (see `renew_sms_subscription`); a failed one is
//! tried again after [`SUBSCRIPTION_RETRY_HOURS`].

use super::JobCaller;
use crate::error::{ApiResult, ErrorBody};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use common::telephony::ringcentral::{SubscriptionOutcome, renew_sms_subscription};
use lambda_http::tracing;
use serde::Serialize;
use sqlx::MySqlPool;
use utoipa::ToSchema;

/// `RingCentral` subscriptions renewed per run (three calls each).
const SUBSCRIPTIONS_PER_RUN: i64 = 2;
/// Before a failed renewal is tried again.
const SUBSCRIPTION_RETRY_HOURS: i64 = 2;

/// What a run did.
#[derive(Debug, Default, Serialize, ToSchema)]
pub struct ContactSyncRun {
    /// Customers synced, both providers.
    pub processed: usize,
    /// Pushed to `RingCentral`.
    pub ringcentral: usize,
    /// Pushed to `CloudTalk`.
    pub cloudtalk: usize,
    /// `RingCentral` SMS subscriptions renewed with the current webhooks
    /// token.
    pub ringcentral_subscriptions: usize,
}

#[derive(Clone, Copy)]
enum Provider {
    Ringcentral,
    Cloudtalk,
}

impl Provider {
    const fn name(self) -> &'static str {
        match self {
            Self::Ringcentral => "ringcentral",
            Self::Cloudtalk => "cloudtalk",
        }
    }

    /// Customers per run. `RingCentral` throttles its contact API hard (and
    /// the call-log sync shares the limit); a sync makes two or three calls.
    const fn per_run(self) -> i64 {
        match self {
            Self::Ringcentral => 3,
            Self::Cloudtalk => 10,
        }
    }

    /// Before a customer still without a contact is tried again; a
    /// `RingCentral` miss is usually only throttling.
    const fn retry_after_hours(self) -> i64 {
        match self {
            Self::Ringcentral => 2,
            Self::Cloudtalk => 24,
        }
    }

    const fn contacts_table(self) -> &'static str {
        match self {
            Self::Ringcentral => "ringcentral_contacts",
            Self::Cloudtalk => "cloudtalk_contacts",
        }
    }

    /// Companies connected to the provider (as the webhooks sync checks).
    const fn connected(self) -> &'static str {
        match self {
            Self::Ringcentral => {
                "((co.ringcentral_access_token IS NOT NULL
                   AND co.ringcentral_refresh_token IS NOT NULL
                   AND co.ringcentral_refresh_expires > UNIX_TIMESTAMP())
                  OR (COALESCE(co.ringcentral_client_id, '') <> ''
                      AND COALESCE(co.ringcentral_client_secret, '') <> ''
                      AND COALESCE(co.ringcentral_jwt, '') <> ''))"
            }
            Self::Cloudtalk => {
                "(COALESCE(co.cloudtalk_access_key, '') <> ''
                  AND COALESCE(co.cloudtalk_access_secret, '') <> '')"
            }
        }
    }
}

/// Customers of connected companies with no contact yet, not tried lately.
async fn due(pool: &MySqlPool, provider: Provider) -> Result<Vec<(i32, i32)>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT c.id, c.company_id
           FROM customers c
           JOIN company co ON co.id = c.company_id
          WHERE c.deleted_at IS NULL AND {connected}
            AND NOT EXISTS (SELECT 1 FROM {contacts} ct WHERE ct.customer_id = c.id)
            AND NOT EXISTS (SELECT 1 FROM contact_sync_attempts a
                             WHERE a.customer_id = c.id AND a.provider = ?
                               AND a.attempted_at > UTC_TIMESTAMP() - INTERVAL ? HOUR)
          ORDER BY c.id DESC
          LIMIT ?",
        connected = provider.connected(),
        contacts = provider.contacts_table(),
    )))
    .bind(provider.name())
    .bind(provider.retry_after_hours())
    .bind(provider.per_run())
    .fetch_all(pool)
    .await
}

async fn sync_provider(state: &AppState, provider: Provider) -> Result<usize, sqlx::Error> {
    let customers = due(&state.pool, provider).await?;
    for &(customer_id, company_id) in &customers {
        // Recorded first, so a customer that breaks the sync is not retried
        // every run.
        sqlx::query(
            "INSERT INTO contact_sync_attempts (customer_id, provider, attempted_at)
             VALUES (?, ?, UTC_TIMESTAMP())
             ON DUPLICATE KEY UPDATE attempted_at = VALUES(attempted_at)",
        )
        .bind(customer_id)
        .bind(provider.name())
        .execute(&state.pool)
        .await?;
        match provider {
            Provider::Ringcentral => {
                state
                    .webhooks
                    .sync_ringcentral_contact(company_id, customer_id)
                    .await;
            }
            Provider::Cloudtalk => {
                state
                    .webhooks
                    .sync_cloudtalk_contact(company_id, customer_id)
                    .await;
            }
        }
    }
    Ok(customers.len())
}

/// `RingCentral`-connected companies whose subscription may carry another
/// token than `token_hash`, not tried lately.
async fn stale_subscriptions(pool: &MySqlPool, token_hash: &str) -> Result<Vec<i32>, sqlx::Error> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT co.id
           FROM company co
          WHERE co.deleted_at IS NULL AND {connected}
            AND (co.ringcentral_subscription_token_hash IS NULL
                 OR co.ringcentral_subscription_token_hash <> ?)
            AND (co.ringcentral_subscription_tried_at IS NULL
                 OR co.ringcentral_subscription_tried_at < UTC_TIMESTAMP() - INTERVAL ? HOUR)
          ORDER BY co.id
          LIMIT ?",
        connected = Provider::Ringcentral.connected(),
    )))
    .bind(token_hash)
    .bind(SUBSCRIPTION_RETRY_HOURS)
    .bind(SUBSCRIPTIONS_PER_RUN)
    .fetch_all(pool)
    .await
}

/// Renews a few subscriptions that may carry an older token.
async fn renew_subscriptions(state: &AppState) -> Result<usize, sqlx::Error> {
    let Some(target) = &state.ringcentral_subscriptions else {
        return Ok(0);
    };
    let mut renewed = 0;
    for company_id in stale_subscriptions(&state.pool, &target.token_hash()).await? {
        // Claimed first, so an overlapping run skips it and a company whose
        // renewal fails is not tried every run.
        let claimed = sqlx::query(
            "UPDATE company SET ringcentral_subscription_tried_at = UTC_TIMESTAMP()
              WHERE id = ?
                AND (ringcentral_subscription_tried_at IS NULL
                     OR ringcentral_subscription_tried_at < UTC_TIMESTAMP() - INTERVAL ? HOUR)",
        )
        .bind(company_id)
        .bind(SUBSCRIPTION_RETRY_HOURS)
        .execute(&state.pool)
        .await?;
        if claimed.rows_affected() == 0 {
            continue;
        }
        match renew_sms_subscription(&state.pool, &state.telephony_http, target, company_id).await {
            Ok(SubscriptionOutcome::Renewed) => renewed += 1,
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(
                    ?error,
                    company_id,
                    "RingCentral SMS subscription renewal failed"
                );
            }
        }
    }
    Ok(renewed)
}

/// Sync missing contacts
///
/// Pushes a few customers without a `RingCentral` or `CloudTalk` contact to
/// their company's provider, and renews up to 2 `RingCentral` SMS
/// subscriptions whose token is not the current webhooks token (a failed
/// renewal waits 2 hours; none is ever created here).
#[utoipa::path(
    post,
    path = "/v1/jobs/contact-sync",
    operation_id = "run_contact_sync",
    tag = "jobs",
    responses(
        (status = 200, description = "What was done", body = ContactSyncRun),
        (status = 401, description = "Missing or wrong job key", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("job_key" = []))
)]
pub async fn run(
    State(state): State<AppState>,
    _job: JobCaller,
) -> ApiResult<Json<ContactSyncRun>> {
    let (ringcentral, cloudtalk, subscriptions) = tokio::join!(
        sync_provider(&state, Provider::Ringcentral),
        sync_provider(&state, Provider::Cloudtalk),
        renew_subscriptions(&state)
    );
    let (ringcentral, cloudtalk) = (ringcentral?, cloudtalk?);
    Ok(Json(ContactSyncRun {
        processed: ringcentral + cloudtalk,
        ringcentral,
        cloudtalk,
        ringcentral_subscriptions: subscriptions?,
    }))
}
