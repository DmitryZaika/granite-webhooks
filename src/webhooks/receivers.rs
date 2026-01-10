use crate::axum_helpers::guards::MarketingUser;
use crate::axum_helpers::guards::{Telegram, TelegramBot};
use crate::libs::leads::process_lead;
use crate::libs::types::BasicResponse;
use crate::schemas::add_customer::{
    FaceBookContactForm, LeadPayload, NewLeadForm, WordpressContactForm,
};
use axum::extract::Path;
use axum::extract::{Json, State};
use sqlx::MySqlPool;

pub async fn wordpress_contact_form(
    _: MarketingUser,
    Path(company_id): Path<i32>,
    State(pool): State<MySqlPool>,
    Json(contact_form): Json<WordpressContactForm>,
) -> BasicResponse {
    let tg_bot = TelegramBot::new();
    new_lead_form_inner(company_id, pool, contact_form, &tg_bot).await
}

pub async fn facebook_contact_form(
    _: MarketingUser,
    Path(company_id): Path<i32>,
    State(pool): State<MySqlPool>,
    Json(contact_form): Json<FaceBookContactForm>,
) -> BasicResponse {
    let tg_bot = TelegramBot::new();
    new_lead_form_inner(company_id, pool, contact_form, &tg_bot).await
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

pub async fn new_lead_form_inner<T, V: LeadPayload>(
    company_id: i32,
    pool: MySqlPool,
    lead_form: V,
    bot: &T,
) -> BasicResponse
where
    T: Telegram + Send + Sync + 'static + Clone,
{
    process_lead(&pool, company_id, &lead_form, bot).await
}

#[cfg(test)]
mod local_tests {
    use super::*;
    use crate::crud::leads::create_deal;
    use crate::tests::telegram::MockTelegram;
    use crate::tests::utils::{insert_user, new_test_app};
    use axum::http::StatusCode;
    use serde_json::Value;
    use serde_json::json;
    use sqlx::MySqlPool;
    use uuid::Uuid;

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

    pub async fn positioned_user(
        pool: &MySqlPool,
        company_id: i32,
        position_id: i32,
        telegram_id: i64,
    ) -> u64 {
        let email = format!("user_{}_email@example.com", Uuid::new_v4());
        let sales_id = insert_user(pool, &email, Some(telegram_id)).await.unwrap();
        assigned_user_position(pool, company_id, position_id, sales_id)
            .await
            .unwrap();
        return sales_id;
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

        positioned_user(&pool, company_id, 1, 123).await;
        positioned_user(&pool, company_id, 2, 456).await;
        positioned_user(&pool, company_id, 2, 789).await;

        let response = new_lead_form_inner(1, pool.clone(), lead, &bot).await;
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

        let sales_id = positioned_user(&pool, company_id, 1, 123).await;
        positioned_user(&pool, company_id, 2, 456).await;
        positioned_user(&pool, company_id, 2, 789).await;

        let response = new_lead_form_inner(1, pool.clone(), lead.clone(), &bot).await;
        assert_eq!(response.0, StatusCode::CREATED);

        let customers = get_customers(&pool).await.unwrap();
        assert_eq!(customers.len(), 1);

        create_deal(&pool, customers[0].id, 1, 0, sales_id as i64)
            .await
            .unwrap();

        let response = new_lead_form_inner(1, pool.clone(), lead, &bot).await;
        assert_eq!(response.0, StatusCode::CREATED);
        let customers = get_customers(&pool).await.unwrap();
        assert_eq!(customers.len(), 1);
        let mut messages = bot.sent.lock().unwrap();
        assert_eq!(messages.len(), 5);

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

        let sales_id = positioned_user(&pool, company_id, 1, 123).await;
        positioned_user(&pool, company_id, 2, 456).await;

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
        let mut messages = bot.sent.lock().unwrap();
        assert_eq!(messages.len(), 3);

        // Assert manager received message
        let second_message = messages.pop().unwrap();
        assert!(second_message.1.starts_with("Repeat lead "));
        assert_eq!(second_message.0, 456);
        // Assert sales recevied message
        let last_message = messages.pop().unwrap();
        assert!(last_message.1.starts_with("You received a REPEATED lead "));
        assert_eq!(last_message.0, 123);
    }

    #[sqlx::test]
    async fn duplicate_lead_notifies_manager_also_sales(pool: MySqlPool) {
        let company_id = 1;
        let data = json!({ "name": "Test", "phone": "+13179995973" });
        let lead: NewLeadForm = serde_json::from_value(data).unwrap();
        let bot = MockTelegram::new();

        let sales_id = positioned_user(&pool, company_id, 1, 123).await;
        let admin_id = positioned_user(&pool, company_id, 2, 456).await;
        assigned_user_position(&pool, company_id, 1, admin_id)
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

        // Assert manager received message
        let second_message = bot.sent.lock().unwrap().pop().unwrap();
        assert!(second_message.1.starts_with("Repeat lead "));
        assert_eq!(second_message.0, 456);
        // Assert sales recevied message
        let last_message = bot.sent.lock().unwrap().pop().unwrap();
        assert!(last_message.1.starts_with("You received a REPEATED lead "));
        assert_eq!(last_message.0, 123);
    }

    #[sqlx::test]
    async fn duplicate_lead_no_deal_existing_customer(pool: MySqlPool) {
        let company_id = 1;
        let data = json!({ "name": "Test", "phone": "13179995973" });
        let lead: FaceBookContactForm = serde_json::from_value(data).unwrap();
        let bot = MockTelegram::new();

        let sales_id = positioned_user(&pool, company_id, 1, 123).await;
        positioned_user(&pool, company_id, 2, 456).await;

        let response = new_lead_form_inner(1, pool.clone(), lead.clone(), &bot).await;
        assert_eq!(response.0, StatusCode::CREATED);
        assert_eq!(bot.sent.lock().unwrap().len(), 1);
        sqlx::query!("UPDATE customers SET sales_rep = ?", sales_id)
            .execute(&pool)
            .await
            .unwrap();

        let response = new_lead_form_inner(1, pool.clone(), lead, &bot).await;
        assert_eq!(response.0, StatusCode::CREATED);

        let customers = get_customers(&pool).await.unwrap();
        assert_eq!(customers.len(), 1);

        println!("{:?}", bot.sent.lock().unwrap());
        let last_message = bot.sent.lock().unwrap().pop().unwrap();
        assert!(
            last_message
                .1
                .starts_with("Repeat lead Test with for sales rep Unknown")
        );
        assert_eq!(last_message.0, 456);

        // Assert manager received message
        let second_message = bot.sent.lock().unwrap().pop().unwrap();
        assert!(
            second_message
                .1
                .starts_with("You received a REPEATED lead Test, click here:")
        );
        assert_eq!(second_message.0, 123);
    }

    #[sqlx::test]
    async fn duplicate_lead_notifies_manager_no_deal_no_existing(pool: MySqlPool) {
        let company_id = 1;
        let data = json!({ "name": "Test", "phone": "+13179995973" });
        let lead: FaceBookContactForm = serde_json::from_value(data).unwrap();
        let bot = MockTelegram::new();

        positioned_user(&pool, company_id, 1, 123).await;
        positioned_user(&pool, company_id, 2, 456).await;

        let response = new_lead_form_inner(1, pool.clone(), lead.clone(), &bot).await;
        assert_eq!(response.0, StatusCode::CREATED);
        assert_eq!(bot.sent.lock().unwrap().len(), 1);

        let customers = get_customers(&pool).await.unwrap();
        assert_eq!(customers.len(), 1);

        let response = new_lead_form_inner(1, pool, lead, &bot).await;
        assert_eq!(response.0, StatusCode::CREATED);
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
}
