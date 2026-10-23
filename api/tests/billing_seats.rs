mod common;

use api::app;
use api::billing::seats::{after_user_change, billable_users, sync_seats};
use api::billing::stripe::signature_header;
use api::state::AppState;
use axum::http::{Method, StatusCode};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession};
use fake_stripe::{FailMode, FakeStripe, Shared, price};
use serde_json::{Value, json};
use sqlx::MySqlPool;

#[path = "support/fake_stripe.rs"]
mod fake_stripe;

const WEBHOOK_SECRET: &str = "whsec_seats_test";
const COMPANY: i32 = 20300;
const MANAGER_SESSION: &str = "aaaaaaaa-0000-4000-8000-000000020300";
const SUBSCRIPTION: &str = "sub_seat";
const ITEMS: &str = "/v1/subscription_items";

struct Fixture {
    pool: MySqlPool,
    state: AppState,
    server: TestServer,
    stripe: Shared,
}

/// A company on `plan_source` with `users` billable users (the manager and
/// the rest as sales reps), and a Stripe subscription with the base item.
async fn fixture(pool: MySqlPool, plan_source: &str, users: i32) -> Fixture {
    let stripe = FakeStripe::with_catalog();
    stripe.lock().unwrap().subscriptions.insert(
        SUBSCRIPTION.into(),
        json!({
            "id": SUBSCRIPTION, "object": "subscription", "customer": "cus_seat",
            "status": "active", "metadata": { "company_id": COMPANY.to_string() },
            "items": { "object": "list", "data": [
                item("si_base", 1, price("price_base", "granite_base_monthly_v1", 30_000)),
            ] },
        }),
    );
    sqlx::query("INSERT INTO company (id, name, plan_source) VALUES (?, 'Seats Stone', ?)")
        .bind(COMPANY)
        .bind(plan_source)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO company_billing
           (company_id, stripe_customer_id, stripe_subscription_id, subscription_status,
            payment_status, billable_users)
         VALUES (?, 'cus_seat', ?, 'active', 'active', ?)",
    )
    .bind(COMPANY)
    .bind(SUBSCRIPTION)
    .bind(users.min(10))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO users (id, email, password, name, is_employee, is_admin, company_id)
         VALUES (20300, 'manager@seats.test', 'x', 'Seat Manager', 1, 1, ?)",
    )
    .bind(COMPANY)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (20300, 5, ?)",
    )
    .bind(COMPANY)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions (id, user_id, expiration_date, is_deleted, created_date)
         VALUES (?, 20300, '2037-12-31 00:00:00', 0, CURRENT_TIMESTAMP)",
    )
    .bind(MANAGER_SESSION)
    .execute(&pool)
    .await
    .unwrap();
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_stripe(fake_stripe::client(&stripe).await)
        .with_stripe_webhook_secret(WEBHOOK_SECRET);
    let server = TestServer::new(app(state.clone()));
    let fixture = Fixture {
        pool,
        state,
        server,
        stripe,
    };
    fixture.add_users(users - 1).await;
    fixture
}

fn item(id: &str, quantity: i64, price: Value) -> Value {
    json!({
        "id": id, "object": "subscription_item", "quantity": quantity, "price": price,
        "current_period_start": 1_798_000_000, "current_period_end": 1_800_600_000,
    })
}

impl Fixture {
    /// `count` more sales reps; answers their ids.
    async fn add_users(&self, count: i32) -> Vec<i32> {
        let next: i32 =
            sqlx::query_scalar("SELECT COALESCE(MAX(id), 20300) + 1 FROM users WHERE id >= 20300")
                .fetch_one(&self.pool)
                .await
                .unwrap();
        let mut ids = Vec::new();
        for id in next..next + count {
            sqlx::query(
                "INSERT INTO users (id, email, password, name, is_employee, company_id)
                 VALUES (?, ?, 'x', 'Seat Rep', 1, ?)",
            )
            .bind(id)
            .bind(format!("rep{id}@seats.test"))
            .bind(COMPANY)
            .execute(&self.pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (?, 1, ?)",
            )
            .bind(id)
            .bind(COMPANY)
            .execute(&self.pool)
            .await
            .unwrap();
            ids.push(id);
        }
        ids
    }

    async fn deactivate(&self, ids: &[i32]) {
        for id in ids {
            sqlx::query("UPDATE users SET is_deleted = 1 WHERE id = ?")
                .bind(id)
                .execute(&self.pool)
                .await
                .unwrap();
        }
    }

    /// A user change as the routes report it.
    async fn changed(&self) {
        after_user_change(&self.state, COMPANY).await;
    }

    fn calls(&self, method: &str, path: &str) -> Vec<fake_stripe::Seen> {
        self.stripe.lock().unwrap().calls(method, path)
    }

    /// Quantity of the extra-user item on the fake subscription, if any.
    fn extra_quantity(&self) -> Option<i64> {
        let stripe = self.stripe.lock().unwrap();
        stripe.subscriptions[SUBSCRIPTION]["items"]["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["price"]["lookup_key"] == "granite_extra_user_monthly_v1")
            .map(|item| item["quantity"].as_i64().unwrap())
    }

    fn item_count(&self) -> usize {
        let stripe = self.stripe.lock().unwrap();
        stripe.subscriptions[SUBSCRIPTION]["items"]["data"]
            .as_array()
            .unwrap()
            .len()
    }

    async fn billing(&self) -> (Option<String>, Option<i64>, i64) {
        sqlx::query_as(
            "SELECT extra_user_item_id, CAST(billable_users AS SIGNED), CAST(seat_change_seq AS SIGNED)
               FROM company_billing WHERE company_id = ?",
        )
        .bind(COMPANY)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    async fn seq(&self) -> i64 {
        self.billing().await.2
    }
}

fn keys(calls: &[fake_stripe::Seen]) -> Vec<String> {
    calls
        .iter()
        .map(|call| call.idempotency_key.clone().unwrap())
        .collect()
}

#[test_db::test(seed)]
async fn seat_pushes_follow_the_billable_count(pool: MySqlPool) {
    let f = fixture(pool, "subscription", 10).await;

    // 10 users: the base covers them, Stripe is not called.
    f.changed().await;
    assert!(
        f.stripe
            .lock()
            .unwrap()
            .requests
            .iter()
            .all(|r| r.path != ITEMS)
    );
    assert_eq!(f.billing().await.0, None);

    // 12: the item is created with quantity 2 under seats:<company>:<seq>.
    f.add_users(2).await;
    f.changed().await;
    let created = f.calls("POST", ITEMS);
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].field("quantity").as_deref(), Some("2"));
    assert_eq!(
        created[0].field("price").as_deref(),
        Some("price_extra_user")
    );
    assert_eq!(
        created[0].idempotency_key.as_deref(),
        Some(format!("seats:{COMPANY}:{}", f.seq().await).as_str())
    );
    assert_eq!(f.extra_quantity(), Some(2));
    let (item_id, billable, _) = f.billing().await;
    assert!(item_id.is_some());
    assert_eq!(billable, Some(12));

    // 13: an update, under a new key.
    let one = f.add_users(1).await;
    f.changed().await;
    assert_eq!(f.extra_quantity(), Some(3));

    // Back to 12 (the 2, 3, 2 case): a third key, so Stripe applies it.
    f.deactivate(&one).await;
    f.changed().await;
    assert_eq!(f.extra_quantity(), Some(2));
    let updates = f.calls(
        "POST",
        &format!("/v1/subscription_items/{}", item_id.clone().unwrap()),
    );
    assert_eq!(
        updates
            .iter()
            .map(|u| u.field("quantity").unwrap())
            .collect::<Vec<_>>(),
        ["3", "2"]
    );
    let mut all = keys(&created);
    all.extend(keys(&updates));
    all.sort();
    all.dedup();
    assert_eq!(all.len(), 3, "every push has its own key");

    // Back to 10: the item is deleted and forgotten.
    let two: Vec<i32> =
        sqlx::query_scalar("SELECT id FROM users WHERE id > 20300 AND is_deleted = 0 LIMIT 2")
            .fetch_all(&f.pool)
            .await
            .unwrap();
    f.deactivate(&two).await;
    f.changed().await;
    assert_eq!(f.extra_quantity(), None);
    assert_eq!(
        f.calls(
            "DELETE",
            &format!("/v1/subscription_items/{}", item_id.unwrap())
        )
        .len(),
        1
    );
    let (item_id, billable, _) = f.billing().await;
    assert_eq!((item_id, billable), (None, Some(10)));
}

#[test_db::test(seed)]
async fn an_unchanged_count_pushes_nothing(pool: MySqlPool) {
    let f = fixture(pool, "subscription", 12).await;
    let first = sync_seats(&f.state, COMPANY).await.unwrap().unwrap();
    assert_eq!(
        (first.billable_users, first.extra_users, first.pushed),
        (12, 2, true)
    );
    let again = sync_seats(&f.state, COMPANY).await.unwrap().unwrap();
    assert!(!again.pushed);
    assert_eq!(f.calls("POST", ITEMS).len(), 1);
}

#[test_db::test(seed)]
async fn system_superuser_and_check_in_only_users_are_not_billable(pool: MySqlPool) {
    let f = fixture(pool, "subscription", 10).await;
    sqlx::raw_sql(
        "INSERT IGNORE INTO positions (id, name) VALUES (8, 'check-in');
         INSERT INTO users (id, email, password, name, is_employee, is_system, company_id)
           VALUES (20340, 'walkin@seats.test', 'x', 'Walk-in', 1, 1, 20300);
         INSERT INTO users (id, email, password, name, is_employee, is_superuser, company_id)
           VALUES (20341, 'super@seats.test', 'x', 'Super', 1, 1, 20300);
         INSERT INTO users (id, email, password, name, is_employee, company_id)
           VALUES (20342, 'checkin@seats.test', 'x', 'Check In', 1, 20300);
         INSERT INTO users_positions (user_id, position_id, company_id) VALUES (20342, 8, 20300);
         INSERT INTO users (id, email, password, name, is_employee, is_deleted, company_id)
           VALUES (20343, 'gone@seats.test', 'x', 'Gone', 1, 1, 20300);",
    )
    .execute(&f.pool)
    .await
    .unwrap();
    assert_eq!(billable_users(&f.pool, COMPANY).await.unwrap(), 10);
    f.changed().await;
    assert!(
        f.stripe
            .lock()
            .unwrap()
            .requests
            .iter()
            .all(|r| r.path != ITEMS)
    );
    // Check-in plus a real position counts.
    sqlx::query(
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (20342, 1, 20300)",
    )
    .execute(&f.pool)
    .await
    .unwrap();
    assert_eq!(billable_users(&f.pool, COMPANY).await.unwrap(), 11);
}

#[test_db::test(seed)]
async fn legacy_companies_are_never_pushed(pool: MySqlPool) {
    let f = fixture(pool, "legacy", 10).await;
    f.add_users(3).await;
    f.changed().await;
    assert!(sync_seats(&f.state, COMPANY).await.unwrap().is_none());
    assert!(f.stripe.lock().unwrap().requests.is_empty());
    assert_eq!(f.seq().await, 0);
}

#[test_db::test(seed)]
async fn a_lost_answer_does_not_create_a_second_item(pool: MySqlPool) {
    let f = fixture(pool, "subscription", 10).await;
    f.add_users(2).await;
    f.stripe
        .lock()
        .unwrap()
        .fail(Method::POST, ITEMS, FailMode::After, 1);
    f.changed().await;
    // The item exists at Stripe, but we never heard.
    assert_eq!(f.extra_quantity(), Some(2));
    assert_eq!(f.billing().await.0, None);
    let synced = sync_seats(&f.state, COMPANY).await.unwrap().unwrap();
    assert_eq!(synced.extra_users, 2);
    assert_eq!(f.item_count(), 2, "base + one extra-user item");
    let (item_id, billable, _) = f.billing().await;
    assert!(item_id.is_some());
    assert_eq!(billable, Some(12));
}

#[test_db::test(seed)]
async fn a_lost_answer_with_a_changed_count_adopts_and_updates(pool: MySqlPool) {
    let f = fixture(pool, "subscription", 10).await;
    f.add_users(2).await;
    f.stripe
        .lock()
        .unwrap()
        .fail(Method::POST, ITEMS, FailMode::After, 1);
    f.changed().await;
    f.add_users(1).await;
    f.changed().await;
    assert_eq!(f.item_count(), 2);
    assert_eq!(f.extra_quantity(), Some(3));
}

#[test_db::test(seed)]
async fn an_outage_leaves_the_state_for_the_next_sync(pool: MySqlPool) {
    let f = fixture(pool, "subscription", 10).await;
    f.add_users(1).await;
    f.stripe
        .lock()
        .unwrap()
        .fail(Method::POST, ITEMS, FailMode::Before, 1);
    f.changed().await;
    assert_eq!(f.extra_quantity(), None);
    assert_eq!(f.billing().await.1, Some(10));
    sync_seats(&f.state, COMPANY).await.unwrap();
    assert_eq!(f.extra_quantity(), Some(1));
}

#[test_db::test(seed)]
async fn the_user_routes_push_seats(pool: MySqlPool) {
    let f = fixture(pool, "subscription", 10).await;
    let res = f
        .server
        .post("/v1/admin/users")
        .with_session(MANAGER_SESSION)
        .json(&json!({
            "name": "New Rep", "email": "new@seats.test", "password": "secret1", "positions": [1],
        }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let id = res.json::<Value>()["id"].as_i64().unwrap();
    assert_eq!(f.extra_quantity(), Some(1));

    f.server
        .delete(&format!("/v1/admin/users/{id}"))
        .with_session(MANAGER_SESSION)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(f.extra_quantity(), None);

    // A Check-in-only account (not billable) given a real position is.
    sqlx::raw_sql(
        "INSERT IGNORE INTO positions (id, name) VALUES (8, 'check-in');
         INSERT INTO users (id, email, password, name, is_employee, company_id)
           VALUES (20350, 'checkin@seats.test', 'x', 'Check In', 1, 20300);
         INSERT INTO users_positions (user_id, position_id, company_id) VALUES (20350, 8, 20300);",
    )
    .execute(&f.pool)
    .await
    .unwrap();
    f.server
        .put("/v1/admin/users/20350")
        .with_session(MANAGER_SESSION)
        .json(&json!({
            "name": "Check In", "email": "checkin@seats.test", "company_id": COMPANY,
            "positions": [1],
        }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(billable_users(&f.pool, COMPANY).await.unwrap(), 11);
    assert_eq!(f.extra_quantity(), Some(1));
}

#[test_db::test(seed)]
async fn a_legacy_company_user_create_pushes_nothing(pool: MySqlPool) {
    let f = fixture(pool, "legacy", 10).await;
    f.server
        .post("/v1/admin/users")
        .with_session(MANAGER_SESSION)
        .json(&json!({
            "name": "New Rep", "email": "new@seats.test", "password": "secret1", "positions": [1],
        }))
        .await
        .assert_status(StatusCode::CREATED);
    assert!(f.stripe.lock().unwrap().requests.is_empty());
}

async fn post_event(f: &Fixture, id: &str, kind: &str) {
    let object = f.stripe.lock().unwrap().subscriptions[SUBSCRIPTION].clone();
    let payload = serde_json::to_vec(&json!({
        "id": id, "type": kind, "data": { "object": object },
    }))
    .unwrap();
    let header = signature_header(&payload, WEBHOOK_SECRET, chrono::Utc::now().timestamp());
    f.server
        .post("/v1/webhooks/stripe")
        .add_header("stripe-signature", header)
        .add_header("content-type", "application/json")
        .bytes(payload.into())
        .await
        .assert_status_ok();
}

fn set_items(f: &Fixture, items: Vec<Value>) {
    f.stripe
        .lock()
        .unwrap()
        .subscriptions
        .get_mut(SUBSCRIPTION)
        .unwrap()["items"]["data"] = Value::Array(items);
}

async fn addon_rows(f: &Fixture) -> Vec<(String, String, Option<String>, bool)> {
    sqlx::query_as(
        "SELECT addon_key, CAST(source AS CHAR), stripe_subscription_item_id, ended_at IS NOT NULL
           FROM company_addons WHERE company_id = ? ORDER BY addon_key, CAST(source AS CHAR)",
    )
    .bind(COMPANY)
    .fetch_all(&f.pool)
    .await
    .unwrap()
}

#[test_db::test(seed)]
async fn subscription_events_sync_items_into_addons(pool: MySqlPool) {
    let f = fixture(pool, "subscription", 12).await;
    sqlx::query(
        "INSERT INTO company_addons (company_id, addon_key, source, started_at)
         VALUES (?, 'quotes', 'comp', '2026-10-01 00:00:00'), (?, 'signatures', 'comp', '2026-10-01 00:00:00')",
    )
    .bind(COMPANY)
    .bind(COMPANY)
    .execute(&f.pool)
    .await
    .unwrap();
    set_items(
        &f,
        vec![
            item(
                "si_base",
                1,
                price("price_base", "granite_base_monthly_v1", 30_000),
            ),
            item(
                "si_quotes",
                1,
                price("price_quotes", "addon_quotes_monthly_v1", 7_500),
            ),
            item(
                "si_extra",
                2,
                price("price_extra_user", "granite_extra_user_monthly_v1", 3_000),
            ),
        ],
    );
    post_event(&f, "evt_1", "customer.subscription.updated").await;

    let rows = addon_rows(&f).await;
    assert_eq!(
        rows,
        vec![
            ("quotes".to_string(), "comp".to_string(), None, false),
            (
                "quotes".to_string(),
                "subscription".to_string(),
                Some("si_quotes".into()),
                false
            ),
            ("signatures".to_string(), "comp".to_string(), None, false),
        ]
    );
    let (item_id, ..) = f.billing().await;
    assert_eq!(item_id.as_deref(), Some("si_extra"));
    let (monthly, users): (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT CAST(monthly_price_cents AS SIGNED), CAST(user_count AS SIGNED) FROM company_billing WHERE company_id = ?",
    )
    .bind(COMPANY)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(monthly, Some(30_000 + 7_500 + 6_000));
    assert_eq!(users, None, "the base item's quantity is not a user count");

    // The same event again changes nothing (started_at is kept).
    let started: String = sqlx::query_scalar(
        "SELECT CAST(started_at AS CHAR) FROM company_addons WHERE company_id = ? AND source = 'subscription'",
    )
    .bind(COMPANY)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    post_event(&f, "evt_2", "customer.subscription.updated").await;
    let again: String = sqlx::query_scalar(
        "SELECT CAST(started_at AS CHAR) FROM company_addons WHERE company_id = ? AND source = 'subscription'",
    )
    .bind(COMPANY)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(started, again);

    // Quotes dropped and the seat item gone: the row ends, the comp rows stay.
    set_items(
        &f,
        vec![item(
            "si_base",
            1,
            price("price_base", "granite_base_monthly_v1", 30_000),
        )],
    );
    post_event(&f, "evt_3", "customer.subscription.updated").await;
    let rows = addon_rows(&f).await;
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows.iter()
            .find(|r| r.0 == "quotes" && r.1 == "subscription")
            .map(|r| r.3),
        Some(true)
    );
    assert!(rows.iter().filter(|r| r.1 == "comp").all(|r| !r.3));
    assert_eq!(f.billing().await.0, None);
}

#[test_db::test(seed)]
async fn a_deleted_subscription_ends_its_addons(pool: MySqlPool) {
    let f = fixture(pool, "subscription", 10).await;
    set_items(
        &f,
        vec![
            item(
                "si_base",
                1,
                price("price_base", "granite_base_monthly_v1", 30_000),
            ),
            item(
                "si_sig",
                1,
                price("price_sig", "addon_signatures_monthly_v1", 1_500),
            ),
            item(
                "si_extra",
                1,
                price("price_extra_user", "granite_extra_user_monthly_v1", 3_000),
            ),
        ],
    );
    post_event(&f, "evt_1", "customer.subscription.updated").await;
    assert_eq!(addon_rows(&f).await.len(), 1);
    f.stripe
        .lock()
        .unwrap()
        .subscriptions
        .get_mut(SUBSCRIPTION)
        .unwrap()["status"] = json!("canceled");
    post_event(&f, "evt_2", "customer.subscription.deleted").await;
    let rows = addon_rows(&f).await;
    assert_eq!(rows.len(), 1);
    assert!(rows[0].3, "ended");
    assert_eq!(f.billing().await.0, None);
}

#[test_db::test(seed)]
async fn a_legacy_single_price_subscription_leaves_addons_alone(pool: MySqlPool) {
    let f = fixture(pool, "legacy", 10).await;
    sqlx::query(
        "INSERT INTO company_addons (company_id, addon_key, source, started_at)
         VALUES (?, 'quotes', 'comp', '2026-10-01 00:00:00')",
    )
    .bind(COMPANY)
    .execute(&f.pool)
    .await
    .unwrap();
    let before = addon_rows(&f).await;
    set_items(
        &f,
        vec![item(
            "si_legacy",
            3,
            json!({ "id": "price_flat", "nickname": "Granite Manager", "unit_amount": 4900, "lookup_key": null }),
        )],
    );
    post_event(&f, "evt_1", "customer.subscription.updated").await;
    assert_eq!(addon_rows(&f).await, before);
    let (monthly, users): (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT CAST(monthly_price_cents AS SIGNED), CAST(user_count AS SIGNED) FROM company_billing WHERE company_id = ?",
    )
    .bind(COMPANY)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!((monthly, users), (Some(14_700), Some(3)));
}
