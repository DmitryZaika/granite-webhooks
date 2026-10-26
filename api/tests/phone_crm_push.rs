//! Pushes of what Telegram also sends: `notification` (a CRM notification,
//! read back with `GET /v1/notifications/{id}`), `lead` and `maintenance`,
//! plus the temporary `POST /v1/phone/pushes/test`.
//!
//! Seed: Rita (100, phone 1) is a sales rep of Acme (100) with deals on Alice
//! Lead (1000); Mark (101, phone 2) is Acme's sales manager.

mod common;
mod phone_common;

use axum::http::StatusCode;
use axum_test::TestServer;
use phone_common::{HEADER, JOB_KEY, Stubs, add_phone, phone_server, sql, token};
use serde_json::{Value, json};
use sqlx::MySqlPool;

async fn setup(pool: &MySqlPool) -> (TestServer, Stubs) {
    let (server, stubs) = phone_server(pool.clone()).await;
    add_phone(pool, 1, 100, 100).await;
    add_phone(pool, 2, 101, 100).await;
    (server, stubs)
}

/// The pushes FCM got of `kind`: (push token, data).
fn pushes(stubs: &Stubs, kind: &str) -> Vec<(String, Value)> {
    stubs
        .pushes()
        .into_iter()
        .filter(|push| push["data"]["type"] == kind)
        .map(|push| {
            (
                push["token"].as_str().unwrap().to_string(),
                push["data"].clone(),
            )
        })
        .collect()
}

async fn job(server: &TestServer, path: &str, body: Value) -> Value {
    let res = server
        .post(path)
        .add_header("authorization", JOB_KEY)
        .json(&body)
        .await;
    res.assert_status_ok();
    res.json()
}

#[test_db::test(seed)]
async fn a_crm_notification_is_pushed_and_read_back(pool: MySqlPool) {
    let (server, stubs) = setup(&pool).await;
    sql(
        &pool,
        "INSERT INTO notifications (id, user_id, deal_id, message, notification_type, actor_name, due_at)
         VALUES (97001, 100, 8000, 'Call back Friday', 'note_added', 'Mark Manager',
                 NOW() - INTERVAL 1 MINUTE)",
    )
    .await;
    let body = json!({ "notification_id": 97001 });
    assert_eq!(
        job(&server, "/v1/phone/pushes/notification", body).await,
        json!({ "pushed": 1 })
    );
    // Ids only, to Rita's phone.
    assert_eq!(
        pushes(&stubs, "notification"),
        [(
            "push-1".to_string(),
            json!({ "granite": "1", "type": "notification", "notificationId": "97001" })
        )]
    );

    let res = server
        .get("/v1/notifications/97001")
        .add_header(HEADER, token(1))
        .await;
    res.assert_status_ok();
    let item: Value = res.json();
    assert_eq!(item["type_title"], "Added a Note");
    assert_eq!(item["customer_name"], "Alice Lead");
    assert_eq!(item["actor_name"], "Mark Manager");
    assert_eq!(item["message"], "Call back Friday");
    assert_eq!(item["deal_id"], 8000);

    // Only its user reads it; an unknown one pushes nothing.
    server
        .get("/v1/notifications/97001")
        .add_header(HEADER, token(2))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    let unknown = json!({ "notification_id": 999_999 });
    assert_eq!(
        job(&server, "/v1/phone/pushes/notification", unknown).await,
        json!({ "pushed": 0 })
    );
}

#[test_db::test(seed)]
async fn lead_alerts_reach_the_managers_or_the_rep(pool: MySqlPool) {
    let (server, stubs) = setup(&pool).await;
    // No user: the sales managers (Mark); the deal is the customer's newest.
    let new = json!({ "company_id": 100, "customer_id": 1001, "kind": "new" });
    assert_eq!(
        job(&server, "/v1/phone/pushes/lead", new).await,
        json!({ "pushed": 1 })
    );
    let assigned = json!({
        "company_id": 100, "customer_id": 1000, "deal_id": 8004, "kind": "assigned", "user_id": 100,
    });
    assert_eq!(
        job(&server, "/v1/phone/pushes/lead", assigned).await,
        json!({ "pushed": 1 })
    );
    assert_eq!(
        pushes(&stubs, "lead"),
        [
            (
                "push-2".to_string(),
                json!({ "granite": "1", "type": "lead", "kind": "new", "customerId": "1001", "dealId": "8001" })
            ),
            (
                "push-1".to_string(),
                json!({ "granite": "1", "type": "lead", "kind": "assigned", "customerId": "1000", "dealId": "8004" })
            ),
        ]
    );

    // Another company's customer: nobody.
    let other = json!({ "company_id": 101, "customer_id": 1000, "kind": "repeat" });
    assert_eq!(
        job(&server, "/v1/phone/pushes/lead", other).await,
        json!({ "pushed": 0 })
    );
    // The key is required.
    server
        .post("/v1/phone/pushes/lead")
        .json(&json!({ "company_id": 100, "customer_id": 1001, "kind": "new" }))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

#[test_db::test(seed)]
async fn the_app_can_send_itself_test_pushes(pool: MySqlPool) {
    let (server, stubs) = setup(&pool).await;
    for kind in ["note_added", "lead_quote", "maintenance"] {
        let res = server
            .post("/v1/phone/pushes/test")
            .add_header(HEADER, token(1))
            .json(&json!({ "kind": kind }))
            .await;
        res.assert_status_ok();
        assert_eq!(res.json::<Value>(), json!({ "pushed": 1 }), "{kind}");
    }
    // The note is a real notification of Rita's, readable by the app.
    let (to, note) = pushes(&stubs, "notification").remove(0);
    assert_eq!(to, "push-1");
    let id = note["notificationId"].as_str().unwrap();
    let res = server
        .get(&format!("/v1/notifications/{id}"))
        .add_header(HEADER, token(1))
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["type_title"], "Added a Note");
    assert_eq!(pushes(&stubs, "lead")[0].1["kind"], "quote");
    assert_eq!(pushes(&stubs, "maintenance")[0].1["count"], "3");
}
