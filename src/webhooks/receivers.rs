use crate::axum_helpers::guards::MarketingUser;
use crate::axum_helpers::guards::{Telegram, TelegramBot};
use crate::crud::leads::LeadForm;
use crate::crud::leads::{
    create_lead_from_facebook, create_lead_from_new_lead_form, create_lead_from_wordpress,
};
use crate::libs::constants::{CREATED_RESPONSE, OK_RESPONSE, internal_error};
use crate::libs::leads::existing_lead_check;
use crate::libs::types::BasicResponse;
use crate::schemas::add_customer::{FaceBookContactForm, NewLeadForm, WordpressContactForm};
use crate::telegram::send::send_telegram_manager_assign;
use axum::extract::Path;
use axum::extract::{Json, State};
use lambda_http::tracing;
use sqlx::MySqlPool;

pub async fn documenso() -> BasicResponse {
    OK_RESPONSE
}

pub async fn wordpress_contact_form(
    _: MarketingUser,
    Path(company_id): Path<i32>,
    State(pool): State<MySqlPool>,
    Json(contact_form): Json<WordpressContactForm>,
) -> BasicResponse {
    let tg_bot = TelegramBot::new();
    wordpress_contact_form_inner(company_id, pool, contact_form, &tg_bot).await
}

pub async fn wordpress_contact_form_inner<T>(
    company_id: i32,
    pool: MySqlPool,
    contact_form: WordpressContactForm,
    bot: &T,
) -> BasicResponse
where
    T: Telegram + Send + Sync + 'static + Clone,
{
    if let Some(response) = existing_lead_check(
        &pool,
        contact_form.email.as_deref(),
        contact_form.phone.as_deref(),
        company_id,
        &LeadForm::WordpressContactForm(contact_form.clone()),
        bot,
    )
    .await
    {
        return response;
    }
    let result = match create_lead_from_wordpress(&pool, &contact_form, company_id).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(?e, "Error creating lead from WordPress");
            return internal_error("Error creating lead from WordPress");
        }
    };
    let tg_result = send_telegram_manager_assign(
        &pool,
        company_id,
        &contact_form.to_string(),
        result.last_insert_id(),
        bot,
    )
    .await;
    if tg_result.is_err() {
        tracing::error!(
            ?tg_result,
            company_id = company_id,
            "Error sending message to Telegram"
        );
        return internal_error("Error sending message to Telegram");
    }
    CREATED_RESPONSE
}

pub async fn facebook_contact_form(
    _: MarketingUser,
    Path(company_id): Path<i32>,
    State(pool): State<MySqlPool>,
    Json(contact_form): Json<FaceBookContactForm>,
) -> BasicResponse {
    let tg_bot = TelegramBot::new();
    facebook_contact_form_inner(company_id, pool, contact_form, &tg_bot).await
}

pub async fn facebook_contact_form_inner<T>(
    company_id: i32,
    pool: MySqlPool,
    contact_form: FaceBookContactForm,
    bot: &T,
) -> BasicResponse
where
    T: Telegram + Send + Sync + 'static + Clone,
{
    if let Some(response) = existing_lead_check(
        &pool,
        contact_form.email.as_deref(),
        contact_form.phone.as_deref(),
        company_id,
        &LeadForm::FaceBookContactForm(contact_form.clone()),
        bot,
    )
    .await
    {
        return response;
    }

    let result = match create_lead_from_facebook(&pool, &contact_form, company_id).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(?e, "Error creating lead from Facebook");
            return internal_error("Error creating lead from Facebook");
        }
    };

    let tg_result = send_telegram_manager_assign(
        &pool,
        company_id,
        &contact_form.to_string(),
        result.last_insert_id(),
        bot,
    )
    .await;
    if tg_result.is_err() {
        tracing::error!(
            ?tg_result,
            company_id = company_id,
            "Error sending message to Telegram"
        );
        return internal_error("Error sending message to Telegram");
    }
    CREATED_RESPONSE
}

pub async fn new_lead_form(
    _: MarketingUser,
    Path(company_id): Path<i32>,
    State(pool): State<MySqlPool>,
    Json(contact_form): Json<NewLeadForm>,
) -> BasicResponse {
    let tg_bot = TelegramBot::new();
    new_lead_form_inner(company_id, pool, contact_form, &tg_bot).await
}

pub async fn new_lead_form_inner<T>(
    company_id: i32,
    pool: MySqlPool,
    contact_form: NewLeadForm,
    bot: &T,
) -> BasicResponse
where
    T: Telegram + Send + Sync + 'static + Clone,
{
    let existing_result = existing_lead_check(
        &pool,
        contact_form.email.as_deref(),
        contact_form.phone.as_deref(),
        company_id,
        &LeadForm::NewLeadForm(contact_form.clone()),
        bot,
    )
    .await;
    if let Some(response) = existing_result {
        return response;
    }

    let result = match create_lead_from_new_lead_form(&pool, &contact_form, company_id).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(?e, "Error creating lead from New Lead Form");
            return internal_error("Error creating lead from New Lead Form");
        }
    };
    let tg_result = send_telegram_manager_assign(
        &pool,
        company_id,
        &contact_form.to_string(),
        result.last_insert_id(),
        bot,
    )
    .await;
    if tg_result.is_err() {
        tracing::error!(
            ?tg_result,
            company_id = company_id,
            "Error sending message to Telegram"
        );
        return internal_error("Error sending message to Telegram");
    }
    CREATED_RESPONSE
}

#[cfg(test)]
mod local_tests {
    use super::*;
    use crate::axum_helpers::axum_app::new_main_app;
    use crate::crud::leads::create_deal;
    use crate::tests::telegram::MockTelegram;
    use crate::tests::utils::insert_user;
    use axum::http::StatusCode;
    use axum_test::TestServer;
    use serde_json::json;
    use sqlx::MySqlPool;

    #[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
    struct Customer {
        id: i32,
        phone: Option<String>,
    }

    async fn get_customers(pool: &MySqlPool) -> Result<Vec<Customer>, sqlx::Error> {
        sqlx::query_as!(
            Customer,
            r#"
            SELECT id, phone
            FROM customers
            "#,
        )
        .fetch_all(pool)
        .await
    }

    pub async fn assigned_user_position(
        pool: &MySqlPool,
        company_id: i32,
        position_id: i32,
        user_id: u64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            r#"
            INSERT INTO positions (id, name)
            VALUES (?, 'Sales')
            ON DUPLICATE KEY UPDATE
                name = VALUES(name)
            "#,
            position_id
        )
        .execute(pool)
        .await?;

        sqlx::query!(
            r#"
            INSERT INTO users_positions (user_id, position_id, company_id)
            VALUES (?, ?, ?)
            ON DUPLICATE KEY UPDATE
                position_id = VALUES(position_id)
            "#,
            user_id,
            position_id,
            company_id
        )
        .execute(pool)
        .await?;

        Ok(())
    }

    fn new_test_app(pool: MySqlPool) -> TestServer {
        let app = new_main_app(pool);
        TestServer::builder().build(app).unwrap()
    }

    use serde_json::Value;

    pub fn lead_payload_json() -> Value {
        serde_json::from_str(
            r#"{
            "zip": "99999",
            "city": "Place",
            "name": "Jack",
            "email": "Jack@gmail.com",
            "phone": "+19999993521",
            "share": "Standard kitchen layout.\nConcerned about existing back splash.",
            "adname": "Ad3 reels social 4500",
            "remove": "yes",
            "campaign": "Indianapolis / LeadAds / Campaign 1",
            "adsetname": "Website + LeadForms VeeGee - 3 lead event"
        }"#,
        )
        .unwrap()
    }

    #[sqlx::test]
    async fn test_basic_facebook(pool: MySqlPool) {
        let app = new_test_app(pool.clone());

        let admin_id = insert_user(&pool, "admin@example.com", Some(456))
            .await
            .unwrap();
        assigned_user_position(&pool, 1, 2, admin_id).await.unwrap();

        let response = app
            .post("/facebook-contact-form/1")
            .json(&lead_payload_json())
            .await;

        assert_eq!(response.status_code(), StatusCode::CREATED);
    }

    #[sqlx::test]
    async fn test_basic_wordpress(pool: MySqlPool) {
        let app = new_test_app(pool.clone());

        let data = json!({ "name": "Test", "Phone": "+13179995973" });
        let lead: WordpressContactForm = serde_json::from_value(data).unwrap();
        let admin_id = insert_user(&pool, "admin@example.com", Some(456))
            .await
            .unwrap();
        assigned_user_position(&pool, 1, 2, admin_id).await.unwrap();

        let response = app.post("/wordpress-contact-form/1").json(&lead).await;

        assert_eq!(response.status_code(), StatusCode::CREATED);
    }

    #[sqlx::test]
    async fn test_basic_new_lead_form(pool: MySqlPool) {
        let app = new_test_app(pool.clone());
        let data = json!({ "name": "Test", "phone": "+13179995973" });
        let lead: NewLeadForm = serde_json::from_value(data).unwrap();

        let admin_id = insert_user(&pool, "admin@example.com", Some(456))
            .await
            .unwrap();
        assigned_user_position(&pool, 1, 2, admin_id).await.unwrap();

        let response = app.post("/v1/webhooks/new-lead-form/1").json(&lead).await;

        assert_eq!(response.status_code(), StatusCode::CREATED);
    }

    #[sqlx::test]
    async fn send_multiple_managers(pool: MySqlPool) {
        let company_id = 1;
        let data = json!({ "name": "Test", "phone": "+13179995973" });
        let lead: NewLeadForm = serde_json::from_value(data).unwrap();
        let bot = MockTelegram::new();

        let sales_id = insert_user(&pool, "colin99delahunty@gmail.com", Some(123))
            .await
            .unwrap();
        assigned_user_position(&pool, company_id, 1, sales_id)
            .await
            .unwrap();

        let admin_id = insert_user(&pool, "admin@example.com", Some(456))
            .await
            .unwrap();
        assigned_user_position(&pool, company_id, 2, admin_id)
            .await
            .unwrap();
        let admin_id2 = insert_user(&pool, "admin2@example.com", Some(789))
            .await
            .unwrap();
        assigned_user_position(&pool, company_id, 2, admin_id2)
            .await
            .unwrap();

        let response = new_lead_form_inner(1, pool.clone(), lead.clone(), &bot).await;
        assert_eq!(response.0, StatusCode::CREATED);
        println!("{:?}", bot.sent.lock().unwrap());
        let mut messages = bot.sent.lock().unwrap();
        assert_eq!(messages.len(), 2);
        let second_message = messages.pop().unwrap();
        assert!(second_message.1.ends_with("Choose a salesperson."));
        assert_eq!(second_message.0, 789);
        let first_message = messages.pop().unwrap();
        assert!(first_message.1.ends_with("Choose a salesperson."));
        assert_eq!(first_message.0, 456);
    }

    #[sqlx::test]
    async fn duplicate_send_multiple_lead_notifies_manager(pool: MySqlPool) {
        let company_id = 1;
        let data = json!({ "name": "Test", "phone": "+13179995973" });
        let lead: NewLeadForm = serde_json::from_value(data).unwrap();
        let bot = MockTelegram::new();

        let sales_id = insert_user(&pool, "colin99delahunty@gmail.com", Some(123))
            .await
            .unwrap();
        assigned_user_position(&pool, company_id, 1, sales_id)
            .await
            .unwrap();

        let admin_id = insert_user(&pool, "admin@example.com", Some(456))
            .await
            .unwrap();
        assigned_user_position(&pool, company_id, 2, admin_id)
            .await
            .unwrap();
        let admin_id2 = insert_user(&pool, "admin2@example.com", Some(789))
            .await
            .unwrap();
        assigned_user_position(&pool, company_id, 2, admin_id2)
            .await
            .unwrap();

        let response = new_lead_form_inner(1, pool.clone(), lead.clone(), &bot).await;
        assert_eq!(response.0, StatusCode::CREATED);

        let customers = get_customers(&pool).await.unwrap();
        assert_eq!(customers.len(), 1);

        create_deal(&pool, customers[0].id, 1, 0, sales_id as i64)
            .await
            .unwrap();

        let response = new_lead_form_inner(1, pool, lead, &bot).await;
        assert_eq!(response.0, StatusCode::CREATED);
        let mut messages = bot.sent.lock().unwrap();
        assert_eq!(messages.len(), 5);

        messages.pop();
        // Assert manager received message
        let second_message = messages.pop().unwrap();
        assert!(second_message.1.starts_with("Repeat lead "));
        assert_eq!(second_message.0, 789);
        let first_message = messages.pop().unwrap();
        assert!(first_message.1.starts_with("Repeat lead "));
        assert_eq!(first_message.0, 456);
    }

    #[sqlx::test]
    async fn duplicate_lead_notifies_manager(pool: MySqlPool) {
        let company_id = 1;
        let data = json!({ "name": "Test", "phone": "+13179995973" });
        let lead: NewLeadForm = serde_json::from_value(data).unwrap();
        let bot = MockTelegram::new();

        let sales_id = insert_user(&pool, "colin99delahunty@gmail.com", Some(123))
            .await
            .unwrap();
        assigned_user_position(&pool, company_id, 1, sales_id)
            .await
            .unwrap();

        let admin_id = insert_user(&pool, "admin@example.com", Some(456))
            .await
            .unwrap();
        assigned_user_position(&pool, company_id, 2, admin_id)
            .await
            .unwrap();

        let response = new_lead_form_inner(1, pool.clone(), lead.clone(), &bot).await;
        assert_eq!(response.0, StatusCode::CREATED);
        assert_eq!(bot.sent.lock().unwrap().len(), 1);

        let customers = get_customers(&pool).await.unwrap();
        assert_eq!(customers.len(), 1);

        create_deal(&pool, customers[0].id, 1, 0, sales_id as i64)
            .await
            .unwrap();

        let response = new_lead_form_inner(1, pool, lead, &bot).await;
        assert_eq!(response.0, StatusCode::CREATED);
        assert_eq!(bot.sent.lock().unwrap().len(), 3);

        // Assert sales recevied message
        let last_message = bot.sent.lock().unwrap().pop().unwrap();
        assert!(last_message.1.starts_with("You received a REPEATED lead "));
        assert_eq!(last_message.0, 123);

        // Assert manager received message
        let second_message = bot.sent.lock().unwrap().pop().unwrap();
        assert!(second_message.1.starts_with("Repeat lead "));
        assert_eq!(second_message.0, 456);
    }

    #[sqlx::test]
    async fn duplicate_lead_notifies_manager_also_sales(pool: MySqlPool) {
        let company_id = 1;
        let data = json!({ "name": "Test", "phone": "+13179995973" });
        let lead: NewLeadForm = serde_json::from_value(data).unwrap();
        let bot = MockTelegram::new();

        let sales_id = insert_user(&pool, "colin99delahunty@gmail.com", Some(123))
            .await
            .unwrap();
        assigned_user_position(&pool, company_id, 1, sales_id)
            .await
            .unwrap();

        let admin_id = insert_user(&pool, "admin@example.com", Some(456))
            .await
            .unwrap();
        assigned_user_position(&pool, company_id, 1, admin_id)
            .await
            .unwrap();
        assigned_user_position(&pool, company_id, 2, admin_id)
            .await
            .unwrap();

        let response = new_lead_form_inner(1, pool.clone(), lead.clone(), &bot).await;
        assert_eq!(response.0, StatusCode::CREATED);
        assert_eq!(bot.sent.lock().unwrap().len(), 1);

        let customers = get_customers(&pool).await.unwrap();
        assert_eq!(customers.len(), 1);

        create_deal(&pool, customers[0].id, 1, 0, sales_id as i64)
            .await
            .unwrap();

        let response = new_lead_form_inner(1, pool, lead, &bot).await;
        assert_eq!(response.0, StatusCode::CREATED);
        assert_eq!(bot.sent.lock().unwrap().len(), 3);

        // Assert sales recevied message
        let last_message = bot.sent.lock().unwrap().pop().unwrap();
        assert!(last_message.1.starts_with("You received a REPEATED lead "));
        assert_eq!(last_message.0, 123);

        // Assert manager received message
        let second_message = bot.sent.lock().unwrap().pop().unwrap();
        assert!(second_message.1.starts_with("Repeat lead "));
        assert_eq!(second_message.0, 456);
    }

    #[sqlx::test]
    async fn facebook_duplicate_lead_notifies_manager(pool: MySqlPool) {
        let company_id = 1;
        let data = json!({ "name": "Test", "phone": "+13179995973" });
        let lead: FaceBookContactForm = serde_json::from_value(data).unwrap();
        let bot = MockTelegram::new();

        let sales_id = insert_user(&pool, "colin99delahunty@gmail.com", Some(123))
            .await
            .unwrap();
        assigned_user_position(&pool, company_id, 1, sales_id)
            .await
            .unwrap();

        let admin_id = insert_user(&pool, "admin@example.com", Some(456))
            .await
            .unwrap();
        assigned_user_position(&pool, company_id, 2, admin_id)
            .await
            .unwrap();

        let response = facebook_contact_form_inner(1, pool.clone(), lead.clone(), &bot).await;
        assert_eq!(response.0, StatusCode::CREATED);
        assert_eq!(bot.sent.lock().unwrap().len(), 1);

        let customers = get_customers(&pool).await.unwrap();
        assert_eq!(customers.len(), 1);

        create_deal(&pool, customers[0].id, 1, 0, sales_id as i64)
            .await
            .unwrap();

        let response = facebook_contact_form_inner(1, pool, lead, &bot).await;
        assert_eq!(response.0, StatusCode::CREATED);
        assert_eq!(bot.sent.lock().unwrap().len(), 3);

        // Assert sales recevied message
        let last_message = bot.sent.lock().unwrap().pop().unwrap();
        assert!(last_message.1.starts_with("You received a REPEATED lead "));
        assert_eq!(last_message.0, 123);

        // Assert manager received message
        let second_message = bot.sent.lock().unwrap().pop().unwrap();
        assert!(second_message.1.starts_with("Repeat lead "));
        assert_eq!(second_message.0, 456);
    }

    #[sqlx::test]
    async fn facebook_duplicate_lead_no_deal_existing_customer(pool: MySqlPool) {
        let company_id = 1;
        let data = json!({ "name": "Test", "phone": "13179995973" });
        let lead: FaceBookContactForm = serde_json::from_value(data).unwrap();
        let bot = MockTelegram::new();

        let sales_id = insert_user(&pool, "colin99delahunty@gmail.com", Some(123))
            .await
            .unwrap();
        assigned_user_position(&pool, company_id, 1, sales_id)
            .await
            .unwrap();

        let admin_id = insert_user(&pool, "admin@example.com", Some(456))
            .await
            .unwrap();
        assigned_user_position(&pool, company_id, 2, admin_id)
            .await
            .unwrap();

        let response = facebook_contact_form_inner(1, pool.clone(), lead.clone(), &bot).await;
        assert_eq!(response.0, StatusCode::CREATED);
        assert_eq!(bot.sent.lock().unwrap().len(), 1);
        let customer = sqlx::query!("UPDATE customers SET sales_rep = ?", sales_id)
            .execute(&pool)
            .await
            .unwrap();

        let response = facebook_contact_form_inner(1, pool.clone(), lead.clone(), &bot).await;

        let customers = get_customers(&pool).await.unwrap();
        assert_eq!(customers.len(), 1);

        println!("{:?}", bot.sent.lock().unwrap());
        let last_message = bot.sent.lock().unwrap().pop().unwrap();
        assert!(last_message.1.starts_with("You received a REPEATED lead "));
        assert_eq!(last_message.0, 123);

        // Assert manager received message
        let second_message = bot.sent.lock().unwrap().pop().unwrap();
        assert!(second_message.1.starts_with("Repeat lead "));
        assert_eq!(second_message.0, 456);
    }

    #[sqlx::test]
    async fn facebook_duplicate_lead_notifies_manager_no_deal_no_existing(pool: MySqlPool) {
        let company_id = 1;
        let data = json!({ "name": "Test", "phone": "+13179995973" });
        let lead: FaceBookContactForm = serde_json::from_value(data).unwrap();
        let bot = MockTelegram::new();

        let sales_id = insert_user(&pool, "colin99delahunty@gmail.com", Some(123))
            .await
            .unwrap();
        assigned_user_position(&pool, company_id, 1, sales_id)
            .await
            .unwrap();

        let admin_id = insert_user(&pool, "admin@example.com", Some(456))
            .await
            .unwrap();
        assigned_user_position(&pool, company_id, 2, admin_id)
            .await
            .unwrap();

        let response = facebook_contact_form_inner(1, pool.clone(), lead.clone(), &bot).await;
        assert_eq!(response.0, StatusCode::CREATED);
        assert_eq!(bot.sent.lock().unwrap().len(), 1);

        let customers = get_customers(&pool).await.unwrap();
        assert_eq!(customers.len(), 1);

        let response = facebook_contact_form_inner(1, pool, lead, &bot).await;
        assert_eq!(response.0, StatusCode::CREATED);
        println!("Response: {:?}", bot.sent.lock().unwrap());
        assert_eq!(bot.sent.lock().unwrap().len(), 2);

        // Assert manager received message
        let second_message = bot.sent.lock().unwrap().pop().unwrap();
        assert!(
            second_message
                .1
                .starts_with("You received a REPEATED lead with no sales rep")
        );
        assert_eq!(second_message.0, 456);
    }

    #[sqlx::test]
    async fn facebook_duplicate_lead_notifies_manager_also_sales(pool: MySqlPool) {
        let company_id = 1;
        let data = json!({ "name": "Test", "phone": "+13179995973" });
        let lead: FaceBookContactForm = serde_json::from_value(data).unwrap();
        let bot = MockTelegram::new();

        let sales_id = insert_user(&pool, "colin99delahunty@gmail.com", Some(123))
            .await
            .unwrap();
        assigned_user_position(&pool, company_id, 1, sales_id)
            .await
            .unwrap();

        let admin_id = insert_user(&pool, "admin@example.com", Some(456))
            .await
            .unwrap();
        assigned_user_position(&pool, company_id, 1, admin_id)
            .await
            .unwrap();
        assigned_user_position(&pool, company_id, 2, admin_id)
            .await
            .unwrap();

        let response = facebook_contact_form_inner(1, pool.clone(), lead.clone(), &bot).await;
        assert_eq!(response.0, StatusCode::CREATED);
        assert_eq!(bot.sent.lock().unwrap().len(), 1);

        let customers = get_customers(&pool).await.unwrap();
        assert_eq!(customers.len(), 1);

        create_deal(&pool, customers[0].id, 1, 0, sales_id as i64)
            .await
            .unwrap();

        let response = facebook_contact_form_inner(1, pool, lead, &bot).await;
        assert_eq!(response.0, StatusCode::CREATED);
        assert_eq!(bot.sent.lock().unwrap().len(), 3);

        // Assert sales recevied message
        let last_message = bot.sent.lock().unwrap().pop().unwrap();
        assert!(last_message.1.starts_with("You received a REPEATED lead "));
        assert_eq!(last_message.0, 123);

        // Assert manager received message
        let second_message = bot.sent.lock().unwrap().pop().unwrap();
        assert!(second_message.1.starts_with("Repeat lead "));
        assert_eq!(second_message.0, 456);
    }

    #[sqlx::test]
    async fn wordpress_duplicate_lead_notifies_manager(pool: MySqlPool) {
        let company_id = 1;
        let data = json!({ "name": "Test", "Phone": "+13179995973" });
        let lead: WordpressContactForm = serde_json::from_value(data).unwrap();
        let bot = MockTelegram::new();

        let sales_id = insert_user(&pool, "colin99delahunty@gmail.com", Some(123))
            .await
            .unwrap();
        assigned_user_position(&pool, company_id, 1, sales_id)
            .await
            .unwrap();

        let admin_id = insert_user(&pool, "admin@example.com", Some(456))
            .await
            .unwrap();
        assigned_user_position(&pool, company_id, 2, admin_id)
            .await
            .unwrap();

        let response = wordpress_contact_form_inner(1, pool.clone(), lead.clone(), &bot).await;
        assert_eq!(response.0, StatusCode::CREATED);
        assert_eq!(bot.sent.lock().unwrap().len(), 1);

        let customers = get_customers(&pool).await.unwrap();
        assert_eq!(customers.len(), 1);

        create_deal(&pool, customers[0].id, 1, 0, sales_id as i64)
            .await
            .unwrap();

        let response = wordpress_contact_form_inner(1, pool, lead, &bot).await;
        assert_eq!(response.0, StatusCode::CREATED);
        assert_eq!(bot.sent.lock().unwrap().len(), 3);

        // Assert sales recevied message
        let last_message = bot.sent.lock().unwrap().pop().unwrap();
        assert!(last_message.1.starts_with("You received a REPEATED lead "));
        assert_eq!(last_message.0, 123);

        // Assert manager received message
        let second_message = bot.sent.lock().unwrap().pop().unwrap();
        assert!(second_message.1.starts_with("Repeat lead "));
        assert_eq!(second_message.0, 456);
    }

    #[sqlx::test]
    async fn wordpress_duplicate_lead_notifies_manager_also_sales(pool: MySqlPool) {
        let company_id = 1;
        let data = json!({ "name": "Test", "Phone": "+13179995973" });
        let lead: WordpressContactForm = serde_json::from_value(data).unwrap();
        let bot = MockTelegram::new();

        let sales_id = insert_user(&pool, "colin99delahunty@gmail.com", Some(123))
            .await
            .unwrap();
        assigned_user_position(&pool, company_id, 1, sales_id)
            .await
            .unwrap();

        let admin_id = insert_user(&pool, "admin@example.com", Some(456))
            .await
            .unwrap();
        assigned_user_position(&pool, company_id, 1, admin_id)
            .await
            .unwrap();
        assigned_user_position(&pool, company_id, 2, admin_id)
            .await
            .unwrap();

        let response = wordpress_contact_form_inner(1, pool.clone(), lead.clone(), &bot).await;
        assert_eq!(response.0, StatusCode::CREATED);
        assert_eq!(bot.sent.lock().unwrap().len(), 1);

        let customers = get_customers(&pool).await.unwrap();
        assert_eq!(customers.len(), 1);

        create_deal(&pool, customers[0].id, 1, 0, sales_id as i64)
            .await
            .unwrap();

        let response = wordpress_contact_form_inner(1, pool, lead, &bot).await;
        assert_eq!(response.0, StatusCode::CREATED);
        assert_eq!(bot.sent.lock().unwrap().len(), 3);

        // Assert sales recevied message
        let last_message = bot.sent.lock().unwrap().pop().unwrap();
        assert!(last_message.1.starts_with("You received a REPEATED lead "));
        assert_eq!(last_message.0, 123);

        // Assert manager received message
        let second_message = bot.sent.lock().unwrap().pop().unwrap();
        assert!(second_message.1.starts_with("Repeat lead "));
        assert_eq!(second_message.0, 456);
    }
}
