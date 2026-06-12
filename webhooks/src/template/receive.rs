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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::utils::*;

    // Seed a company
    async fn insert_test_company(
        pool: &MySqlPool,
        name: &str,
        address: Option<&str>,
    ) -> Result<i32, sqlx::Error> {
        let rec = sqlx::query!(
            r#"INSERT INTO company (name, address) VALUES (?, ?)"#,
            name,
            address
        )
        .execute(pool)
        .await?;
        Ok(rec.last_insert_id() as i32)
    }

    // Seed a user (assuming schema fields match UserVariableData + company_id)
    async fn insert_test_user(
        pool: &MySqlPool,
        company_id: i32,
        name: Option<&str>,
        email: &str,
        phone: Option<&str>,
    ) -> Result<i32, sqlx::Error> {
        let rec = sqlx::query!(
            r#"INSERT INTO users (company_id, name, email, phone_number) VALUES (?, ?, ?, ?)"#,
            company_id,
            name,
            email,
            phone
        )
        .execute(pool)
        .await?;
        Ok(rec.last_insert_id() as i32)
    }

    // Seed a customer
    async fn insert_test_customer(
        pool: &MySqlPool,
        company_id: Option<i32>,
        name: &str,
        address: Option<&str>,
    ) -> Result<i32, sqlx::Error> {
        let rec = sqlx::query!(
            r#"INSERT INTO customers (company_id, name, address, deleted_at) VALUES (?, ?, ?, NULL)"#,
            company_id,
            name,
            address
        )
        .execute(pool)
        .await?;
        Ok(rec.last_insert_id() as i32)
    }

    // Seed a deal linked to a customer
    async fn insert_test_deal(pool: &MySqlPool, customer_id: i32) -> Result<i32, sqlx::Error> {
        let group_id = insert_group_list(pool, 1).await.unwrap();
        let list_id = insert_deals_list(pool, group_id).await.unwrap();
        let rec = sqlx::query!(
            r#"INSERT INTO deals (customer_id, deleted_at, list_id, position) VALUES (?, NULL, ?, 0)"#,
            customer_id,
            list_id
        )
        .execute(pool)
        .await?;
        Ok(rec.last_insert_id() as i32)
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn test_fetch_template_all_data_present(pool: MySqlPool) {
        let company_id = insert_test_company(&pool, "Enterprise Corp", Some("123 Main St"))
            .await
            .unwrap();
        let user_id = insert_test_user(
            &pool,
            company_id,
            Some("Alice"),
            "alice@test.com",
            Some("555-1234"),
        )
        .await
        .unwrap();
        let customer_id = insert_test_customer(
            &pool,
            Some(company_id),
            "Acme Client",
            Some("456 Market St"),
        )
        .await
        .unwrap();
        let deal_id = insert_test_deal(&pool, customer_id).await.unwrap();

        let result = fetch_template_variable_data(&pool, user_id, Some(deal_id), Some(customer_id))
            .await
            .unwrap();

        // Assert User
        assert_eq!(result.user.name, Some("Alice".to_string()));
        assert_eq!(result.user.email, Some("alice@test.com".to_string()));

        // Assert Customer (via Deal Join)
        let customer = result.customer.unwrap();
        assert_eq!(customer.name, Some("Acme Client".to_string()));
        assert_eq!(customer.address, Some("456 Market St".to_string()));

        // Assert Company
        let company = result.company.unwrap();
        assert_eq!(company.name, Some("Enterprise Corp".to_string()));
        assert_eq!(company.address, Some("123 Main St".to_string()));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn test_fetch_template_minimal_user_only(pool: MySqlPool) {
        let company_id = insert_test_company(&pool, "Enterprise Corp", None)
            .await
            .unwrap();
        let user_id = insert_test_user(&pool, company_id, Some("Bob"), "alice@test.com", None)
            .await
            .unwrap();

        let result = fetch_template_variable_data(&pool, user_id, None, None)
            .await
            .unwrap();

        assert_eq!(result.user.name, Some("Bob".to_string()));
        assert!(result.customer.is_none());
        assert!(result.company.is_some());
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn test_fetch_customer_fallback_to_customer_id(pool: MySqlPool) {
        let company_id = insert_test_company(&pool, "Company A", None).await.unwrap();
        let user_id = insert_test_user(&pool, company_id, Some("Charlie"), "alice@test.com", None)
            .await
            .unwrap();
        let customer_id =
            insert_test_customer(&pool, Some(company_id), "Direct Customer", Some("789 Lane"))
                .await
                .unwrap();

        // Use a non-existent deal_id (99999) to force the deal block to bypass/fail selection
        let result = fetch_template_variable_data(&pool, user_id, Some(99999), Some(customer_id))
            .await
            .unwrap();

        let customer = result
            .customer
            .expect("Should fall back to customer_id lookup");
        assert_eq!(customer.name, Some("Direct Customer".to_string()));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn test_customer_and_deal_soft_deletes_ignored(pool: MySqlPool) {
        let company_id = insert_test_company(&pool, "Company B", None).await.unwrap();
        let user_id = insert_test_user(&pool, company_id, Some("David"), "alice@test.com", None)
            .await
            .unwrap();
        let customer_id = insert_test_customer(&pool, Some(company_id), "Deleted Customer", None)
            .await
            .unwrap();
        let deal_id = insert_test_deal(&pool, customer_id).await.unwrap();

        // Soft delete the deal
        sqlx::query!(
            r#"UPDATE deals SET deleted_at = NOW() WHERE id = ?"#,
            deal_id
        )
        .execute(&pool)
        .await
        .unwrap();

        // Soft delete the customer
        sqlx::query!(
            r#"UPDATE customers SET deleted_at = NOW() WHERE id = ?"#,
            customer_id
        )
        .execute(&pool)
        .await
        .unwrap();

        let result = fetch_template_variable_data(&pool, user_id, Some(deal_id), Some(customer_id))
            .await
            .unwrap();

        // Customer object should return None because both lookups hit soft-deleted walls
        assert!(result.customer.is_none());
    }
    #[sqlx::test(migrations = "../migrations")]
    async fn test_company_isolation_mismatch_returns_none(pool: MySqlPool) {
        let company_id_1 = insert_test_company(&pool, "Company One", None)
            .await
            .unwrap();
        let company_id_2 = insert_test_company(&pool, "Company Two", None)
            .await
            .unwrap();

        // User belongs to Company One
        let user_id = insert_test_user(&pool, company_id_1, Some("Eve"), "alice@test.com", None)
            .await
            .unwrap();

        // Customer and Deal belong to Company Two
        let customer_id = insert_test_customer(&pool, Some(company_id_2), "Rival Client", None)
            .await
            .unwrap();
        let deal_id = insert_test_deal(&pool, customer_id).await.unwrap();

        let result = fetch_template_variable_data(&pool, user_id, Some(deal_id), Some(customer_id))
            .await
            .unwrap();

        // Should be None because query enforces c.company_id = user.company_id
        assert!(result.customer.is_none());
    }
}
