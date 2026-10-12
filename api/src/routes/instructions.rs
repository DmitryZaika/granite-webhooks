//! The company's instructions (how-to pages), kept as a tree whose siblings
//! are chained by `after_id`.
//!
//! - `PUT /v1/instructions/order` → save a drag-and-drop order of siblings
//!
//! Replaces the Remix `api.instructions.reorder` route.

use crate::auth::AdminUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::state::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use std::collections::HashSet;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

/// The AI follow-up settings live in `instructions` but are not shown in the tree.
const AI_FOLLOWUP_TITLE: &str = "AI follow-up instruction";

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(order))
}

/// Which instructions to reorder.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum InstructionScope {
    /// The caller's company.
    #[default]
    Company,
    /// Shared by every company (super admins only; others get `company`).
    General,
}

/// Body of `PUT /v1/instructions/order`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct InstructionOrder {
    /// The parent whose children are reordered; null for the top level.
    pub parent_id: Option<i32>,
    /// Every child of that parent, in the new order.
    pub ordered_ids: Vec<i32>,
    /// `company` (default) or `general`.
    #[serde(default)]
    pub scope: InstructionScope,
}

/// Order instructions
///
/// Saves the order of one parent's children; the list must hold each of
/// them exactly once.
#[utoipa::path(
    put,
    path = "/v1/instructions/order",
    operation_id = "order_instructions",
    tag = "instructions",
    request_body = InstructionOrder,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Empty, repeated, missing or foreign ids", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn order(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<InstructionOrder>,
) -> ApiResult<StatusCode> {
    let company_id = if user.is_superuser && input.scope == InstructionScope::General {
        0
    } else {
        user.company_id
    };
    let ordered = &input.ordered_ids;
    let unique: HashSet<i32> = ordered.iter().copied().collect();
    if ordered.is_empty() || unique.len() != ordered.len() {
        return Err(ApiError::BadRequest(
            "List every instruction of the parent once".into(),
        ));
    }
    let siblings: HashSet<i32> = sqlx::query_scalar(
        "SELECT id FROM instructions
          WHERE company_id = ? AND (title IS NULL OR title <> ?) AND parent_id <=> ?",
    )
    .bind(company_id)
    .bind(AI_FOLLOWUP_TITLE)
    .bind(input.parent_id)
    .fetch_all(&state.pool)
    .await?
    .into_iter()
    .collect();
    if siblings != unique {
        return Err(ApiError::BadRequest(
            "List every instruction of the parent once".into(),
        ));
    }
    let mut tx = state.pool.begin().await?;
    let mut after = None;
    for id in ordered {
        sqlx::query("UPDATE instructions SET after_id = ? WHERE id = ? AND company_id = ?")
            .bind(after)
            .bind(id)
            .bind(company_id)
            .execute(&mut *tx)
            .await?;
        after = Some(*id);
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
