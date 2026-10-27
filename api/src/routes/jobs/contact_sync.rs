//! `POST /v1/jobs/contact-sync`: every customer into the company's
//! `RingCentral` and `CloudTalk` contacts.
//!
//! Edits already sync right away; this catches customers created elsewhere
//! (lead forms, imports, inbound texts) and syncs that failed. Each run takes
//! a few customers without a contact per provider; one that is still missing
//! after its try waits a while (`Provider::retry_after_hours`) for the next.

use crate::auth::job::JobCaller;
use crate::error::{ApiResult, ErrorBody};
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::Serialize;
use sqlx::MySqlPool;

/// What a run did.
#[derive(Debug, Default, Serialize, ToSchema)]
pub struct ContactSyncRun {
    /// Customers synced, both providers.
    pub processed: usize,
    /// Pushed to `RingCentral`.
    pub ringcentral: usize,
    /// Pushed to `CloudTalk`.
    pub cloudtalk: usize,
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

/// Sync missing contacts
///
/// Pushes a few customers without a `RingCentral` or `CloudTalk` contact to
/// their company's provider.
#[crate::openapi::path(
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
    let (ringcentral, cloudtalk) = tokio::join!(
        sync_provider(&state, Provider::Ringcentral),
        sync_provider(&state, Provider::Cloudtalk)
    );
    let (ringcentral, cloudtalk) = (ringcentral?, cloudtalk?);
    Ok(Json(ContactSyncRun {
        processed: ringcentral + cloudtalk,
        ringcentral,
        cloudtalk,
    }))
}
