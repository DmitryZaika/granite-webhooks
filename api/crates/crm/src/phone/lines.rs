//! The company's Telnyx numbers (`telnyx_numbers`) and who may use them
//! (`telnyx_number_users`): the caller IDs a user may call from.
//!
//! An assignment counts only when the user is in the number's company
//! (`JOIN users u ON u.company_id = n.company_id`), whatever the table says.

use super::wire_enum;
use crate::openapi::ToSchema;
use serde::Serialize;
use sqlx::{FromRow, MySqlPool};

wire_enum! {
    /// What a company number is for: `direct` (one person's line), `shared`
    /// (a team line) or `main` (the company's main line).
    pub enum NumberKind {
        /// One person's own line.
        Direct = "direct",
        /// A line several people answer.
        Shared = "shared",
        /// The company's main line.
        Main = "main",
    }
}

/// A company number the user may call from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CallerId {
    /// The number (`telnyx_numbers.id`): send it as `callerNumberId`.
    pub number_id: i32,
    /// The number, E.164 (`+13175550101`).
    pub e164: String,
    /// Its label, e.g. "Main line"; may be empty.
    pub label: String,
    /// `direct`, `shared` or `main`.
    #[sqlx(try_from = "String")]
    pub kind: NumberKind,
}

/// The active numbers `user_id` may call from, their own direct lines
/// first, then shared lines, then the main line (each by label).
pub async fn caller_ids(
    pool: &MySqlPool,
    company_id: i32,
    user_id: i32,
) -> sqlx::Result<Vec<CallerId>> {
    sqlx::query_as(
        "SELECT n.id AS number_id, n.e164, n.label, CAST(n.kind AS CHAR) AS kind
           FROM telnyx_numbers n
           JOIN telnyx_number_users nu ON nu.number_id = n.id AND nu.user_id = ? AND nu.can_call_from <> 0
           JOIN users u ON u.id = nu.user_id AND u.company_id = n.company_id AND u.is_deleted = 0
          WHERE n.company_id = ? AND n.active <> 0
          ORDER BY FIELD(n.kind, 'direct', 'shared', 'main'), n.label, n.id",
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_all(pool)
    .await
}

/// The caller ID a call goes out with when none is picked: the user's
/// chosen default while it is still one of theirs, else their first direct
/// line, else the first number they may use.
pub fn default_caller_id(caller_ids: &[CallerId], chosen: Option<i32>) -> Option<&CallerId> {
    chosen
        .and_then(|id| caller_ids.iter().find(|caller| caller.number_id == id))
        .or_else(|| {
            caller_ids
                .iter()
                .find(|caller| caller.kind == NumberKind::Direct)
        })
        .or_else(|| caller_ids.first())
}

/// The user's line for calls and texts from the CRM: their chosen default
/// caller ID while it is still theirs, else their direct number, else the
/// first number they may call from.
pub async fn user_line(
    pool: &MySqlPool,
    company_id: i32,
    user_id: i32,
) -> sqlx::Result<Option<CallerId>> {
    let ids = caller_ids(pool, company_id, user_id).await?;
    let chosen: Option<i32> = sqlx::query_scalar(
        "SELECT default_caller_number_id FROM phone_user_settings
          WHERE user_id = ? AND company_id = ?",
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?
    .flatten();
    Ok(default_caller_id(&ids, chosen).cloned())
}

/// A number of the company our server can call from when the user has no
/// caller ID of their own (test calls, cell codes): the main line first,
/// then any active number.
pub async fn company_number(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Option<CallerId>> {
    sqlx::query_as(
        "SELECT n.id AS number_id, n.e164, n.label, CAST(n.kind AS CHAR) AS kind
           FROM telnyx_numbers n
          WHERE n.company_id = ? AND n.active <> 0
          ORDER BY FIELD(n.kind, 'main', 'shared', 'direct'), n.id
          LIMIT 1",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await
}
