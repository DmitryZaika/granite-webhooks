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
    #[serde(alias = "dealId")]
    pub deal_id: Option<i32>,
    #[serde(alias = "customerId")]
    pub customer_id: Option<i32>,
}

#[derive(Deserialize)]
pub struct TemplatePayload {
    template: String,
}

pub async fn get_template_variables(
    _: RemixBackend,
    State(pool): State<MySqlPool>,
    Path((company_id, user_id)): Path<(i32, i32)>,
    Query(query): Query<TemplateDataQuery>,
) -> Result<Json<TemplateVariableData>, BasicResponse> {
    match fetch_template_variable_data(&pool, user_id, query.deal_id, query.customer_id, company_id)
        .await
    {
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
    Path((company_id, user_id)): Path<(i32, i32)>,
    Query(query): Query<TemplateDataQuery>,
    extract::Json(payload): extract::Json<TemplatePayload>,
) -> impl IntoResponse {
    let data = match fetch_template_variable_data(
        &pool,
        user_id,
        query.deal_id,
        query.customer_id,
        company_id,
    )
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

        let result = fetch_template_variable_data(
            &pool,
            user_id,
            Some(deal_id),
            Some(customer_id),
            company_id,
        )
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

        let result = fetch_template_variable_data(&pool, user_id, None, None, company_id)
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
        let result = fetch_template_variable_data(
            &pool,
            user_id,
            Some(99999),
            Some(customer_id),
            company_id,
        )
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

        let result = fetch_template_variable_data(
            &pool,
            user_id,
            Some(deal_id),
            Some(customer_id),
            company_id,
        )
        .await
        .unwrap();

        // Customer object should return None because both lookups hit soft-deleted walls
        assert!(result.customer.is_none());
    }
    // -------------------------------------------------------------------------
    // Template variable replacement tests (pure function, no DB required)
    // -------------------------------------------------------------------------

    use common::crud::template::{InfoVariableData, UserVariableData};

    fn template_path(filename: &str) -> String {
        std::fs::read_to_string(
            concat!(env!("CARGO_MANIFEST_DIR"), "/src/tests/data/",).to_string() + filename,
        )
        .expect("Failed to read template file")
    }

    /// Builds a fully-populated TemplateVariableData for replacement tests.
    fn make_full_data() -> TemplateVariableData {
        TemplateVariableData {
            user: UserVariableData {
                name: Some("Alice Johnson".to_string()),
                email: Some("alice@test.com".to_string()),
                phone_number: Some("555-1234".to_string()),
            },
            customer: Some(InfoVariableData {
                name: Some("Acme Client".to_string()),
                address: Some("456 Market St".to_string()),
            }),
            company: Some(InfoVariableData {
                name: Some("Granite Depot".to_string()),
                address: Some("123 Main St".to_string()),
            }),
        }
    }

    /// Builds minimal data: only user with a single-word name, no customer, no company.
    fn make_minimal_data() -> TemplateVariableData {
        TemplateVariableData {
            user: UserVariableData {
                name: Some("Bob".to_string()),
                email: Some("bob@test.com".to_string()),
                phone_number: None,
            },
            customer: None,
            company: None,
        }
    }

    // --- Tests for each template ---

    #[test]
    fn test_replace_template_countertop_approval() {
        let template = template_path("template_countertop_approval.html");
        let data = make_full_data();
        let result = replace_template_variables(&template, &data);

        // customer.first_name → "Acme" (first word of "Acme Client")
        assert!(result.contains("Hi Acme"));
        // user.name → "Alice Johnson"
        assert!(result.contains(">Alice Johnson<"));
        // company.name → "Granite Depot"
        assert!(result.contains(">Granite Depot<"));
        // Unsupported placeholder remains unchanged
        assert!(result.contains("{{Also, you invoice will change by ...if applicable...}}"));
        // Original placeholders are gone
        assert!(!result.contains("{{customer.first_name}}"));
        assert!(!result.contains("{{user.name}}"));
        assert!(!result.contains("{{company.name}}"));
    }

    #[test]
    fn test_replace_template_inventory_link() {
        let template = template_path("template_inventory_link.html");
        let data = make_full_data();
        let result = replace_template_variables(&template, &data);

        assert!(result.contains("Hi Acme"));
        assert!(result.contains("this is Alice"));
        assert!(result.contains("with Granite Depot"));
        assert!(!result.contains("{{customer.first_name}}"));
        assert!(!result.contains("{{user.first_name}}"));
        assert!(!result.contains("{{company.name}}"));
    }

    #[test]
    fn test_replace_template_countertop_attached() {
        let template = template_path("template_countertop_attached.html");
        let data = make_full_data();
        let result = replace_template_variables(&template, &data);

        assert!(result.contains("Hi Acme"));
        assert!(!result.contains("{{customer.first_name}}"));
        assert!(result.contains("{{Also, your invoice will change"));
    }

    #[test]
    fn test_replace_template_backsplash() {
        let template = template_path("template_backsplash.html");
        let data = make_full_data();
        let result = replace_template_variables(&template, &data);

        assert!(result.contains("Hi Acme"));
        assert!(!result.contains("{{customer.first_name}}"));
        assert!(result.contains("{{Also, your invoice will change by ...if applicable...}}"));
    }

    #[test]
    fn test_replace_template_countertop_faucet() {
        let template = template_path("template_countertop_faucet.html");
        let data = make_full_data();
        let result = replace_template_variables(&template, &data);

        assert!(result.contains("Hi Acme"));
        assert!(!result.contains("{{customer.first_name}}"));
        assert!(result.contains("{{Also, your invoice will change by ...if applicable...}}"));
    }

    #[test]
    fn test_replace_template_contract_signing() {
        let template = template_path("template_contract_signing.html");
        let data = make_full_data();
        let result = replace_template_variables(&template, &data);

        assert!(result.contains("Hi Acme"));
        assert!(result.contains(">Alice Johnson<"));
        assert!(result.contains(">Granite Depot<"));
        assert!(!result.contains("{{customer.first_name}}"));
        assert!(!result.contains("{{user.name}}"));
        assert!(!result.contains("{{company.name}}"));
    }

    #[test]
    fn test_replace_template_pickup_project() {
        let template = template_path("template_pickup_project.html");
        let data = make_full_data();
        let result = replace_template_variables(&template, &data);

        assert!(result.contains("Hi Acme"));
        assert!(!result.contains("{{customer.first_name}}"));
    }

    #[test]
    fn test_replace_template_contract_signing_v2() {
        let template = template_path("template_contract_signing_v2.html");
        let data = make_full_data();
        let result = replace_template_variables(&template, &data);

        assert!(result.contains("Hi Acme"));
        assert!(!result.contains("{{customer.first_name}}"));
    }

    #[test]
    fn test_replace_template_follow_up() {
        let template = template_path("template_follow_up.html");
        let data = make_full_data();
        let result = replace_template_variables(&template, &data);

        assert!(result.contains("Hi Acme"));
        assert!(result.contains("This is Alice"));
        assert!(result.contains("with Granite Depot"));
        assert!(!result.contains("{{customer.first_name}}"));
        assert!(!result.contains("{{user.first_name}}"));
        assert!(!result.contains("{{company.name}}"));
    }

    #[test]
    fn test_replace_template_curated_options() {
        let template = template_path("template_curated_options.html");
        let data = make_full_data();
        let result = replace_template_variables(&template, &data);

        assert!(result.contains("Hi Acme"));
        assert!(result.contains("This is Alice"));
        assert!(result.contains("with Granite Depot"));
        assert!(!result.contains("{{customer.first_name}}"));
        assert!(!result.contains("{{user.first_name}}"));
        assert!(!result.contains("{{company.name}}"));
    }

    #[test]
    fn test_replace_template_initial_contact() {
        let template = template_path("template_initial_contact.html");
        let data = make_full_data();
        let result = replace_template_variables(&template, &data);

        assert!(result.contains("Hi Acme"));
        assert!(result.contains("contacting Granite Depot"));
        assert!(result.contains("My name is Alice"));
        assert!(!result.contains("{{customer.first_name}}"));
        assert!(!result.contains("{{user.first_name}}"));
        assert!(!result.contains("{{company.name}}"));
    }

    #[test]
    fn test_replace_template_final_payment() {
        let template = template_path("template_final_payment.html");
        let data = make_full_data();
        let result = replace_template_variables(&template, &data);

        assert!(result.contains("Hi Acme"));
        assert!(!result.contains("{{customer.first_name}}"));
        // {{payment_link}} is not a supported variable, so it stays
        assert!(result.contains("{{payment_link}}"));
    }

    #[test]
    fn test_replace_template_dear_customer() {
        let template = template_path("template_dear_customer.html");
        let data = make_full_data();
        let result = replace_template_variables(&template, &data);

        // "Dear Customer" is literal text, no variable replacement needed
        assert!(result.contains("Dear Customer"));
        // Template should be unchanged since it has no supported variables
        assert_eq!(result, template);
    }

    // --- Edge-case tests ---

    #[test]
    fn test_replace_empty_template() {
        let data = make_full_data();
        let result = replace_template_variables("", &data);
        assert_eq!(result, "");
    }

    #[test]
    fn test_replace_no_variables_in_template() {
        let template = "<p>Hello World</p>";
        let data = make_full_data();
        let result = replace_template_variables(template, &data);
        assert_eq!(result, template);
    }

    #[test]
    fn test_replace_unknown_variables_left_untouched() {
        let template = "<p>{{unknown.var}} and {{Salesrep name}} stay as-is</p>";
        let data = make_full_data();
        let result = replace_template_variables(template, &data);
        assert!(result.contains("{{unknown.var}}"));
        assert!(result.contains("{{Salesrep name}}"));
    }

    #[test]
    fn test_replace_all_data_missing() {
        // User has a name, but customer and company are None → their variables stay
        let data = make_minimal_data();
        let template = "<p>{{user.first_name}}|{{customer.first_name}}|{{company.name}}|{{user.phone_number}}</p>";
        let result = replace_template_variables(template, &data);

        // user.first_name → "Bob"
        assert!(result.contains("Bob|"));
        // customer.first_name stays (customer is None)
        assert!(result.contains("{{customer.first_name}}"));
        // company.name stays (company is None)
        assert!(result.contains("{{company.name}}"));
        // user.phone_number stays (it is None)
        assert!(result.contains("{{user.phone_number}}"));
    }

    #[test]
    fn test_replace_variable_appears_multiple_times() {
        let template = "{{user.name}} and again {{user.name}}";
        let data = make_full_data();
        let result = replace_template_variables(template, &data);
        assert_eq!(result, "Alice Johnson and again Alice Johnson");
        assert!(!result.contains("{{user.name}}"));
    }

    #[test]
    fn test_replace_variable_substring_of_another_variable() {
        // "user.name" is a substring of "user.first_name" placeholder text.
        // The full placeholder "{{user.first_name}}" must not be partially matched.
        let template = "{{user.name}} vs {{user.first_name}}";
        let data = make_full_data();
        let result = replace_template_variables(template, &data);
        // user.name → "Alice Johnson", user.first_name → "Alice"
        assert_eq!(result, "Alice Johnson vs Alice");
    }

    #[test]
    fn test_replace_current_date_is_populated() {
        let template = "<p>Date: {{current_date}}</p>";
        let data = make_full_data();
        let result = replace_template_variables(template, &data);

        // current_date should be replaced with something like "June 12"
        assert!(!result.contains("{{current_date}}"));
        // The replaced value should not be empty
        let date_part = result
            .strip_prefix("<p>Date: ")
            .unwrap()
            .strip_suffix("</p>")
            .unwrap();
        assert!(!date_part.is_empty());
    }

    #[test]
    fn test_replace_customer_address_and_company_address() {
        let template = "<p>{{customer.address}} | {{company.address}}</p>";
        let data = make_full_data();
        let result = replace_template_variables(template, &data);
        assert_eq!(result, "<p>456 Market St | 123 Main St</p>");
    }

    #[test]
    fn test_replace_user_email_and_phone() {
        let template = "<p>{{user.email}} | {{user.phone_number}}</p>";
        let data = make_full_data();
        let result = replace_template_variables(template, &data);
        assert_eq!(result, "<p>alice@test.com | 555-1234</p>");
    }

    #[test]
    fn test_first_name_extracts_only_first_word() {
        // "Alice Johnson" → first_name should be "Alice"
        let template = "{{user.first_name}}";
        let data = make_full_data();
        let result = replace_template_variables(template, &data);
        assert_eq!(result, "Alice");
    }

    #[test]
    fn test_customer_first_name_with_multi_word_name() {
        let mut data = make_full_data();
        data.customer = Some(InfoVariableData {
            name: Some("Acme Client Corp".to_string()),
            address: None,
        });
        let template = "{{customer.first_name}}";
        let result = replace_template_variables(template, &data);
        assert_eq!(result, "Acme");
    }

    #[test]
    fn test_empty_user_name_produces_no_first_name() {
        let mut data = make_full_data();
        data.user.name = Some("".to_string());
        let template = "{{user.name}}:{{user.first_name}}";
        let result = replace_template_variables(template, &data);
        // Empty string is filtered out by value.filter(|s| !s.is_empty())
        // So both placeholders remain unreplaced
        assert_eq!(result, "{{user.name}}:{{user.first_name}}");
    }

    #[test]
    fn test_replace_with_special_html_characters() {
        let mut data = make_full_data();
        data.company = Some(InfoVariableData {
            name: Some("Granite Depot & Sons".to_string()),
            address: None,
        });
        let template = "<p>{{company.name}}</p>";
        let result = replace_template_variables(template, &data);
        // The ampersand is not HTML-escaped by replace_template_variables
        assert_eq!(result, "<p>Granite Depot & Sons</p>");
    }

    // -------------------------------------------------------------------------
    // Query parameter deserialization tests (ensure camelCase from frontend works)
    // -------------------------------------------------------------------------

    /// Verify that camelCase query params (`dealId`, `customerId`) sent by the
    /// Remix frontend are correctly deserialized into snake_case struct fields.
    #[test]
    fn test_query_deserializes_camel_case_params() {
        let query = "dealId=2396&customerId=100213";
        let params: TemplateDataQuery = serde_urlencoded::from_str(query).unwrap();
        assert_eq!(params.deal_id, Some(2396));
        assert_eq!(params.customer_id, Some(100213));
    }

    /// Verify that snake_case query params (used in internal/scheduled flows)
    /// are also accepted for backward compatibility.
    #[test]
    fn test_query_deserializes_snake_case_params() {
        let query = "deal_id=2396&customer_id=100213";
        let params: TemplateDataQuery = serde_urlencoded::from_str(query).unwrap();
        assert_eq!(params.deal_id, Some(2396));
        assert_eq!(params.customer_id, Some(100213));
    }

    /// When no query params are provided, both fields should be None.
    #[test]
    fn test_query_deserializes_empty_params() {
        let query = "";
        let params: TemplateDataQuery = serde_urlencoded::from_str(query).unwrap();
        assert_eq!(params.deal_id, None);
        assert_eq!(params.customer_id, None);
    }

    /// When only dealId is provided, customer_id should be None.
    #[test]
    fn test_query_deserializes_deal_id_only() {
        let query = "dealId=42";
        let params: TemplateDataQuery = serde_urlencoded::from_str(query).unwrap();
        assert_eq!(params.deal_id, Some(42));
        assert_eq!(params.customer_id, None);
    }

    /// When only customerId is provided, deal_id should be None.
    #[test]
    fn test_query_deserializes_customer_id_only() {
        let query = "customerId=7";
        let params: TemplateDataQuery = serde_urlencoded::from_str(query).unwrap();
        assert_eq!(params.deal_id, None);
        assert_eq!(params.customer_id, Some(7));
    }
}
