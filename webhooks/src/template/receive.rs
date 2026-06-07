use crate::axum_helpers::guards::RemixBackend;
use crate::libs::types::BasicResponse;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{
    Json,
    extract::{self, Path, Query, State},
};
use common::{
    crud::template::{TemplateVariableData, fetch_template_variable_data},
    utils::template::replace_template_variables,
};
use lambda_http::tracing;
use serde::Deserialize;
use sqlx::MySqlPool;

use crate::libs::constants::{ERR_DB, NOT_FOUND_RESPONSE, internal_error};

#[derive(Deserialize)]
pub struct TemplateDataQuery {
    pub deal_id: Option<i32>,
    pub customer_id: Option<i32>,
}

#[derive(Deserialize)]
pub struct TemplatePayload {
    template: String,
}

pub async fn get_template_variables(
    _: RemixBackend,
    State(pool): State<MySqlPool>,
    Path(user_id): Path<i32>,
    Query(query): Query<TemplateDataQuery>,
) -> Result<Json<TemplateVariableData>, BasicResponse> {
    match fetch_template_variable_data(&pool, user_id, query.deal_id, query.customer_id).await {
        Ok(data) => Ok(Json(data)),
        Err(sqlx::Error::RowNotFound) => Err(NOT_FOUND_RESPONSE),
        Err(error) => {
            tracing::error!(
                "Error fetching template variable data for user_id {}: {}",
                user_id,
                error
            );
            Err(internal_error(ERR_DB))
        }
    }
}

pub async fn get_complete_template(
    _: RemixBackend,
    State(pool): State<MySqlPool>,
    Path(user_id): Path<i32>,
    Query(query): Query<TemplateDataQuery>,
    extract::Json(payload): extract::Json<TemplatePayload>,
) -> impl IntoResponse {
    let data = match fetch_template_variable_data(&pool, user_id, query.deal_id, query.customer_id)
        .await
    {
        Ok(data) => data,
        Err(sqlx::Error::RowNotFound) => return NOT_FOUND_RESPONSE.into_response(),
        Err(error) => {
            tracing::error!(
                "Error fetching template variable data for user_id {}: {}",
                user_id,
                error
            );
            return internal_error(ERR_DB).into_response();
        }
    };
    let result = replace_template_variables(&payload.template, &data);
    (StatusCode::OK, result).into_response()
}
