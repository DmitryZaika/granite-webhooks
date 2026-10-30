//! A company with a two-list deal board and one lead on it, for the tests of
//! deal moves (`crud::deals`) and of the call webhooks that make them.

use crate::tests::utils::{insert_group_list, insert_user};
use sqlx::MySqlPool;
use uuid::Uuid;

pub struct BoardFixture {
    pub company_id: i32,
    pub user_id: i32,
    pub customer_id: i32,
    pub deal_id: u64,
    pub first_list_id: i32,
    pub second_list_id: i32,
    pub customer_email: String,
}

pub async fn insert_company(pool: &MySqlPool) -> i32 {
    let rec = sqlx::query!(r#"INSERT INTO company (name) VALUES ('Move Company')"#)
        .execute(pool)
        .await
        .unwrap();
    i32::try_from(rec.last_insert_id()).unwrap()
}

pub async fn insert_list(pool: &MySqlPool, group_id: u64, name: &str, position: i32) -> i32 {
    let rec = sqlx::query!(
        r#"INSERT INTO deals_list (name, group_id, position) VALUES (?, ?, ?)"#,
        name,
        group_id,
        position
    )
    .execute(pool)
    .await
    .unwrap();
    i32::try_from(rec.last_insert_id()).unwrap()
}

pub async fn setup_board(pool: &MySqlPool, phone: Option<&str>) -> BoardFixture {
    let company_id = insert_company(pool).await;
    let email = format!("rep_{}@example.com", Uuid::new_v4());
    let user_id = insert_user(pool, &email, None).await.unwrap();
    sqlx::query!(
        r#"UPDATE users SET company_id = ? WHERE id = ?"#,
        company_id,
        user_id
    )
    .execute(pool)
    .await
    .unwrap();

    let group_id = insert_group_list(pool, company_id).await.unwrap();
    let first_list_id = insert_list(pool, group_id, "Not Contacted Yet", 0).await;
    let second_list_id = insert_list(pool, group_id, "Contacted", 1).await;

    let customer_email = format!("lead_{}@example.com", Uuid::new_v4());
    let customer = sqlx::query!(
        r#"INSERT INTO customers (name, company_id, phone, source) VALUES ('Lead', ?, ?, 'leads')"#,
        company_id,
        phone
    )
    .execute(pool)
    .await
    .unwrap();
    let customer_id = i32::try_from(customer.last_insert_id()).unwrap();
    let email_row = sqlx::query!(
        r#"INSERT INTO customers_emails (customer_id, email) VALUES (?, ?)"#,
        customer_id,
        customer_email
    )
    .execute(pool)
    .await
    .unwrap();
    let email_id = i32::try_from(email_row.last_insert_id()).unwrap();
    sqlx::query!(
        r#"UPDATE customers SET email_id = ? WHERE id = ?"#,
        email_id,
        customer_id
    )
    .execute(pool)
    .await
    .unwrap();

    let deal = sqlx::query!(
        r#"INSERT INTO deals (customer_id, status, list_id, position, user_id) VALUES (?, 'Not Contacted Yet', ?, 0, ?)"#,
        customer_id,
        first_list_id,
        user_id
    )
    .execute(pool)
    .await
    .unwrap();

    sqlx::query!(
        r#"INSERT INTO deal_stage_history (deal_id, list_id) VALUES (?, ?)"#,
        deal.last_insert_id(),
        first_list_id
    )
    .execute(pool)
    .await
    .unwrap();

    BoardFixture {
        company_id,
        user_id,
        customer_id,
        deal_id: deal.last_insert_id(),
        first_list_id,
        second_list_id,
        customer_email,
    }
}

pub async fn deal_list_id(pool: &MySqlPool, deal_id: u64) -> i32 {
    sqlx::query_scalar!(r#"SELECT list_id FROM deals WHERE id = ?"#, deal_id)
        .fetch_one(pool)
        .await
        .unwrap()
}
