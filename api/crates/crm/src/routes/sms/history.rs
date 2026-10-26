//! The texts with a customer, for a deal's or customer's activity panel.
//!
//! Replaces the Remix `api.{cloudtalk,ringcentral}.{dealSms,customerSms}`
//! routes.

use super::provider::{Param, SmsProvider, fetch_all, marks, phone_variants};
use super::schemas::Attachment;
use super::threads::attachments;
use crate::auth::EmployeeUser;
use crate::error::{ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::openapi::{IntoParams, ToSchema};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};

/// Newest texts returned.
const LIMIT: i64 = 200;

/// Query of `GET /v1/sms/{provider}/history`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct HistoryQuery {
    /// The deal's customer's texts since the deal was created.
    pub deal_id: Option<i64>,
    /// Or: every text with these phones (repeat the parameter).
    #[serde(default)]
    pub phone: Vec<String>,
}

/// One text.
#[derive(Debug, Serialize, ToSchema)]
pub struct HistoryText {
    /// The SMS row.
    pub id: i32,
    /// The provider's message id; null for Telnyx (its ids are not numbers).
    pub provider_id: Option<i64>,
    /// Sending phone (digits as stored).
    pub sender: Option<String>,
    /// Receiving phone.
    pub recipient: String,
    /// The message.
    pub text: String,
    /// The rep's name when the agent id is known, else the raw agent id.
    pub agent: Option<String>,
    /// The agent id stored on the row.
    pub agent_id: Option<String>,
    /// When it was stored.
    pub created_date: Option<DateTime<Utc>>,
    /// How many photos and files were sent with it.
    pub attachment_count: i64,
    /// The photos and files sent with it, in the order they were sent.
    pub attachments: Vec<Attachment>,
}

/// `GET /v1/sms/{provider}/history`.
#[derive(Debug, Serialize, ToSchema)]
pub struct History {
    /// Newest first, at most 200.
    pub items: Vec<HistoryText>,
    /// The customer's phones (with and without the country code); a text
    /// from one of them is inbound.
    pub customer_phone_digits: Vec<String>,
}

#[derive(sqlx::FromRow)]
struct Row {
    id: i32,
    provider_id: Option<i64>,
    sender: Option<String>,
    recipient: String,
    text: String,
    agent: Option<String>,
    created_date: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow)]
struct DealPhones {
    phone: Option<String>,
    phone_2: Option<String>,
    created_at: Option<DateTime<Utc>>,
}

/// The phones of a deal's customer and when the deal was made.
async fn deal_phones(
    pool: &sqlx::MySqlPool,
    company_id: i32,
    deal_id: i64,
) -> Result<Option<DealPhones>, sqlx::Error> {
    sqlx::query_as(
        "SELECT c.phone, c.phone_2, d.created_at FROM deals d
           JOIN customers c ON c.id = d.customer_id
          WHERE d.id = ? AND c.company_id = ? AND d.deleted_at IS NULL",
    )
    .bind(deal_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await
}

/// Rep names by agent (or extension) id; by user id on Telnyx.
async fn agent_names(
    pool: &sqlx::MySqlPool,
    provider: SmsProvider,
    company_id: i32,
) -> Result<HashMap<String, String>, sqlx::Error> {
    let Some(column) = provider.agent_column() else {
        return crate::phone::texting::user_names(pool, company_id).await;
    };
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT TRIM({column}), MIN(name) FROM users
          WHERE company_id = ? AND is_deleted = 0 AND {column} IS NOT NULL AND TRIM({column}) != ''
          GROUP BY TRIM({column})"
    )))
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, name)| Some((id, name?)))
        .collect())
}

fn digits(phone: &str) -> Option<String> {
    let digits: String = phone.chars().filter(char::is_ascii_digit).collect();
    (digits.len() >= 10).then_some(digits)
}

/// List a customer's texts
///
/// With `deal_id`: texts with the deal's customer since the deal was made
/// (none for a deal of another company). With `phone`: every text with
/// those phones. Echoes of the company's own sends and unsent texts are
/// left out.
#[crate::openapi::path(
    get,
    path = "/v1/sms/{provider}/history",
    operation_id = "list_sms_history",
    tag = "sms",
    params(("provider" = SmsProvider, Path, description = "SMS provider"), HistoryQuery),
    responses(
        (status = 200, description = "Texts", body = History),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn history(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(provider): Path<SmsProvider>,
    MultiQuery(query): MultiQuery<HistoryQuery>,
) -> ApiResult<Json<History>> {
    let pool = &state.pool;
    let (phones, since) = match query.deal_id {
        Some(deal_id) => match deal_phones(pool, user.company_id, deal_id).await? {
            Some(deal) => (
                [deal.phone, deal.phone_2].into_iter().flatten().collect(),
                deal.created_at,
            ),
            None => (Vec::new(), None),
        },
        None => (query.phone, None),
    };
    let variants: Vec<String> = phones
        .iter()
        .filter_map(|phone| digits(phone))
        .flat_map(|phone| phone_variants(&phone))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if variants.is_empty() {
        return Ok(Json(History {
            items: Vec::new(),
            customer_phone_digits: Vec::new(),
        }));
    }
    let table = provider.sms_table();
    // Telnyx's ids are UUIDs: none here.
    let provider_id = if provider.numeric_ids() {
        format!("s.{}", provider.id_column())
    } else {
        "CAST(NULL AS SIGNED)".to_string()
    };
    let list = marks(variants.len());
    let mut params: Vec<Param> = vec![user.company_id.into()];
    params.extend(variants.iter().map(Param::from));
    params.extend(variants.iter().map(Param::from));
    let since_clause = since.map_or(String::new(), |at| {
        params.push(at.format("%Y-%m-%d %H:%M:%S").to_string().into());
        " AND s.created_date >= ?".to_string()
    });
    params.push(LIMIT.into());
    let rows: Vec<Row> = fetch_all(
        pool,
        format!(
            "SELECT s.id, {provider_id} AS provider_id, CAST(s.sender AS CHAR) AS sender,
                    CAST(s.recipient AS CHAR) AS recipient, s.text, s.agent, s.created_date
               FROM {table} s
              WHERE s.company_id = ? AND s.status IN ('received', 'sent')
                AND (s.sender IN ({list}) OR s.recipient IN ({list}))
                AND s.is_echo = 0{since_clause}
              ORDER BY s.created_date DESC
              LIMIT ?"
        ),
        &params,
    )
    .await?;
    let names = agent_names(pool, provider, user.company_id).await?;
    let ids: Vec<i32> = rows.iter().map(|row| row.id).collect();
    let mut files = attachments(pool, provider, &ids).await?;
    let items = rows
        .into_iter()
        .map(|row| {
            let attachments = files.remove(&row.id).unwrap_or_default();
            let agent_id = row
                .agent
                .as_deref()
                .map(str::trim)
                .filter(|agent| !agent.is_empty())
                .map(str::to_string);
            HistoryText {
                agent: agent_id
                    .as_ref()
                    .map(|id| names.get(id).cloned().unwrap_or_else(|| id.clone())),
                agent_id,
                id: row.id,
                provider_id: row.provider_id,
                sender: row.sender,
                recipient: row.recipient,
                text: row.text,
                created_date: row.created_date,
                attachment_count: attachments.len() as i64,
                attachments,
            }
        })
        .collect();
    Ok(Json(History {
        items,
        customer_phone_digits: variants,
    }))
}
