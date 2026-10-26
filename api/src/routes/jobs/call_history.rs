//! `POST /v1/jobs/call-history`: every `CloudTalk` and `RingCentral` call
//! into `provider_calls`, so a company's call history stays on its deals and
//! customers after it changes provider.
//!
//! Each company and provider reads its call log forward from where the last
//! run stopped (`provider_call_syncs`), less a day for calls that ended late.
//! The first runs walk it a month at a time from the company's first
//! customer: several months a run for `CloudTalk`, one for `RingCentral`,
//! whose call log allows few requests a minute.

use crate::auth::job::JobCaller;
use crate::error::{ApiResult, ErrorBody};
use crate::openapi::ToSchema;
use crate::routes::calls::ringcentral::map_record;
use crate::routes::calls::saved;
use crate::routes::company::queries::company_features;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use common::telephony::{cloudtalk, ringcentral};
use lambda_http::tracing;
use reqwest::Method;
use serde::Serialize;
use serde_json::Value;

const CLOUDTALK_PAGE: usize = 1000;
const MAX_PAGES: usize = 10;
/// Re-read a day before where the last run stopped.
const LOOKBACK_HOURS: i64 = 24;
const WINDOW_DAYS: i64 = 31;

/// What a run did.
#[derive(Debug, Default, Serialize, ToSchema)]
pub struct CallHistoryRun {
    /// Calls saved or refreshed, both providers.
    pub processed: usize,
}

#[derive(Clone, Copy)]
enum Provider {
    CloudTalk,
    RingCentral,
}

impl Provider {
    const fn name(self) -> &'static str {
        match self {
            Self::CloudTalk => "cloudtalk",
            Self::RingCentral => "ringcentral",
        }
    }

    /// Months of call log one run reads while catching up.
    const fn windows_per_run(self) -> usize {
        match self {
            Self::CloudTalk => 6,
            Self::RingCentral => 1,
        }
    }
}

fn encode(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
}

async fn cloudtalk_calls(
    state: &AppState,
    company_id: i32,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<Value>, cloudtalk::ApiError> {
    let api = cloudtalk::Api {
        pool: &state.pool,
        client: &state.telephony_http,
        base_url: &state.cloudtalk_base,
        company_id,
    };
    let at = |time: DateTime<Utc>| encode(&time.format("%Y-%m-%d %H:%M:%S").to_string());
    let mut items = Vec::new();
    for page in 1..=MAX_PAGES {
        let body = cloudtalk::request(
            api,
            Method::GET,
            &format!(
                "calls/index.json?date_from={}&date_to={}&limit={CLOUDTALK_PAGE}&page={page}",
                at(from),
                at(to)
            ),
            None,
            None,
        )
        .await?;
        let data = body["responseData"]["data"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let full = data.len() >= CLOUDTALK_PAGE;
        items.extend(data);
        if !full {
            break;
        }
    }
    Ok(items)
}

async fn ringcentral_calls(
    state: &AppState,
    company_id: i32,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<Value>, ringcentral::ApiError> {
    let at =
        |time: DateTime<Utc>| encode(&time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
    let mut path = format!(
        "account/~/call-log?view=Detailed&perPage=1000&dateFrom={}&dateTo={}",
        at(from),
        at(to)
    );
    let mut items = Vec::new();
    for _ in 0..MAX_PAGES {
        let body = ringcentral::request(
            &state.pool,
            &state.telephony_http,
            company_id,
            Method::GET,
            &path,
            &ringcentral::Body::Empty,
            None,
        )
        .await?;
        items.extend(
            body["records"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|record| map_record(record, None)),
        );
        // The next page is a full URI; keep what follows the API root.
        let Some(next) = body["navigation"]["nextPage"]["uri"]
            .as_str()
            .and_then(|uri| {
                uri.split_once("/restapi/v1.0/")
                    .map(|(_, rest)| rest.to_string())
            })
        else {
            break;
        };
        path = next;
    }
    Ok(items)
}

/// Where the company's call log is read from: where the last run stopped,
/// else its first customer (at most five years back).
async fn read_from(
    state: &AppState,
    company_id: i32,
    provider: Provider,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>, sqlx::Error> {
    let synced_to: Option<NaiveDateTime> = sqlx::query_scalar(
        "SELECT synced_to FROM provider_call_syncs WHERE company_id = ? AND provider = ?",
    )
    .bind(company_id)
    .bind(provider.name())
    .fetch_optional(&state.pool)
    .await?;
    if let Some(synced_to) = synced_to {
        return Ok(synced_to.and_utc() - Duration::hours(LOOKBACK_HOURS));
    }
    let first: Option<NaiveDateTime> = sqlx::query_scalar(
        "SELECT MIN(created_date) FROM customers WHERE company_id = ? AND created_date > '2000-01-01'",
    )
    .bind(company_id)
    .fetch_one(&state.pool)
    .await?;
    let earliest = now - Duration::days(5 * 365);
    Ok(first
        .map(|first| first.and_utc())
        .filter(|first| *first > earliest)
        .unwrap_or(earliest))
}

async fn sync_company(
    state: &AppState,
    company_id: i32,
    provider: Provider,
) -> Result<usize, String> {
    let now = Utc::now();
    let mut from = read_from(state, company_id, provider, now)
        .await
        .map_err(|err| err.to_string())?;
    let mut saved_calls = 0;
    for _ in 0..provider.windows_per_run() {
        let to = (from + Duration::days(WINDOW_DAYS)).min(now);
        let items = match provider {
            Provider::CloudTalk => cloudtalk_calls(state, company_id, from, to)
                .await
                .map_err(|err| format!("cloudtalk {}", err.status))?,
            Provider::RingCentral => ringcentral_calls(state, company_id, from, to)
                .await
                .map_err(|err| format!("ringcentral {}", err.status))?,
        };
        saved_calls += saved::save(&state.pool, company_id, provider.name(), &items)
            .await
            .map_err(|err| err.to_string())?;
        sqlx::query(
            "INSERT INTO provider_call_syncs (company_id, provider, synced_to) VALUES (?, ?, ?)
             ON DUPLICATE KEY UPDATE synced_to = VALUES(synced_to)",
        )
        .bind(company_id)
        .bind(provider.name())
        .bind(to.naive_utc())
        .execute(&state.pool)
        .await
        .map_err(|err| err.to_string())?;
        if to >= now {
            break;
        }
        from = to;
    }
    Ok(saved_calls)
}

/// Saves every connected company's new calls.
pub async fn sync(state: &AppState) -> Result<usize, sqlx::Error> {
    let companies: Vec<i32> = sqlx::query_scalar("SELECT id FROM company")
        .fetch_all(&state.pool)
        .await?;
    let mut processed = 0;
    for company_id in companies {
        let features = company_features(&state.pool, company_id).await?;
        for (provider, connected) in [
            (Provider::CloudTalk, features.cloudtalk),
            (Provider::RingCentral, features.ringcentral),
        ] {
            if !connected {
                continue;
            }
            match sync_company(state, company_id, provider).await {
                Ok(count) => processed += count,
                // Rate limited or the keys no longer work: the next run tries again.
                Err(err) => {
                    tracing::info!(company_id, provider = provider.name(), error = %err, "call history sync skipped");
                }
            }
        }
    }
    Ok(processed)
}

/// Save call history
///
/// Saves the `CloudTalk` and `RingCentral` calls of every connected company,
/// so they stay in its history after it changes provider.
#[crate::openapi::path(
    post,
    path = "/v1/jobs/call-history",
    operation_id = "run_call_history",
    tag = "jobs",
    responses(
        (status = 200, description = "What was done", body = CallHistoryRun),
        (status = 401, description = "Missing or wrong job key", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("job_key" = []))
)]
pub async fn run(
    State(state): State<AppState>,
    _job: JobCaller,
) -> ApiResult<Json<CallHistoryRun>> {
    let processed = sync(&state).await?;
    Ok(Json(CallHistoryRun { processed }))
}
