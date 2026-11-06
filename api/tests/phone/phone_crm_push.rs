//! Pushes of what Telegram also sends: `notification` (a CRM notification,
//! read back with `GET /v1/notifications/{id}`), `lead` and `maintenance`,
//! plus the temporary `POST /v1/phone/pushes/test`.
//!
//! Seed: Rita (100, phone 1) is a sales rep of Acme (100) with deals on Alice
//! Lead (1000); Mark (101, phone 2) is Acme's sales manager.

use crate::phone_common;

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
    // Its id and heading, to Rita's phone.
    assert_eq!(
        pushes(&stubs, "notification"),
        [(
            "push-1".to_string(),
            json!({ "granite": "1", "type": "notification", "notificationId": "97001",
                    "heading": "Added a Note" })
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
    assert_eq!(item["customer_id"], 1000);

    // A finished installation of a customer without a deal opens the customer.
    sql(
        &pool,
        "INSERT INTO notifications (id, user_id, customer_id, message, notification_type, due_at)
         VALUES (97002, 100, 1000, 'Alice Lead is installed', 'installation_completed',
                 NOW() - INTERVAL 1 MINUTE)",
    )
    .await;
    let res = server
        .get("/v1/notifications/97002")
        .add_header(HEADER, token(1))
        .await;
    res.assert_status_ok();
    let item: Value = res.json();
    assert_eq!(item["customer_id"], 1000);
    assert!(item.get("deal_id").is_none());

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
    // A repeated lead: the managers' copy opens the Assign sheet, the rep's does not.
    let repeat_managers = json!({
        "company_id": 100, "customer_id": 1000, "deal_id": 8004, "kind": "repeat",
    });
    assert_eq!(
        job(&server, "/v1/phone/pushes/lead", repeat_managers).await,
        json!({ "pushed": 1 })
    );
    let repeat_rep = json!({
        "company_id": 100, "customer_id": 1000, "deal_id": 8004, "kind": "repeat", "user_id": 100,
    });
    assert_eq!(
        job(&server, "/v1/phone/pushes/lead", repeat_rep).await,
        json!({ "pushed": 1 })
    );
    assert_eq!(
        pushes(&stubs, "lead"),
        [
            (
                "push-2".to_string(),
                json!({ "granite": "1", "type": "lead", "kind": "new", "customerId": "1001", "dealId": "8001", "assign": "1" })
            ),
            (
                "push-1".to_string(),
                json!({ "granite": "1", "type": "lead", "kind": "assigned", "customerId": "1000", "dealId": "8004" })
            ),
            (
                "push-2".to_string(),
                json!({ "granite": "1", "type": "lead", "kind": "repeat", "customerId": "1000", "dealId": "8004", "assign": "1" })
            ),
            (
                "push-1".to_string(),
                json!({ "granite": "1", "type": "lead", "kind": "repeat", "customerId": "1000", "dealId": "8004" })
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
    // A maintenance tap lists the checks: the app reads them with its token.
    server
        .get("/v1/shop/maintenance")
        .add_header(HEADER, token(1))
        .await
        .assert_status_ok();
}

/// A Telegram that records each call: ("<bot path segment>/<method>", body).
async fn telegram_stub() -> (
    String,
    std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>>,
) {
    use axum::extract::Path;
    use axum::routing::post;
    use axum::{Json, Router};
    use std::sync::{Arc, Mutex};
    let chats: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
    let recorded = chats.clone();
    let telegram = Router::new().route(
        "/{bot}/{method}",
        post(
            move |Path((bot, method)): Path<(String, String)>, Json(body): Json<Value>| {
                let recorded = recorded.clone();
                async move {
                    let mut list = recorded.lock().unwrap();
                    list.push((format!("{bot}/{method}"), body));
                    Json(json!({ "ok": true, "result": { "message_id": 800 + list.len() } }))
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, telegram).await.unwrap() });
    (base, chats)
}

#[test_db::test(seed)]
async fn a_lead_assigned_from_the_phone_reaches_the_rep_in_the_app_and_in_telegram(
    pool: MySqlPool,
) {
    use api::telegram::TelegramBots;
    let (telegram, chats) = telegram_stub().await;
    let (server, stubs) = phone_common::phone_server_with(pool.clone(), |state| {
        state.with_telegram(TelegramBots::new(
            Some("111:leads"),
            Some("222:notes"),
            &telegram,
        ))
    })
    .await;
    add_phone(&pool, 1, 100, 100).await;
    add_phone(&pool, 2, 101, 100).await;
    // Rita has the app and Telegram's lead alerts.
    sql(&pool, "UPDATE users SET telegram_id = 5551 WHERE id = 100").await;
    // Mark, the sales manager, got the lead in Telegram: once with its text
    // kept, once before texts were kept.
    sql(&pool, "UPDATE users SET telegram_id = 7771 WHERE id = 101").await;
    sql(
        &pool,
        "INSERT INTO telegram_lead_messages (customer_id, company_id, chat_id, message_id, text)
         VALUES (1001, 100, 7771, 41, 'New lead: Bob Unassigned\nChoose a salesperson.'),
                (1001, 100, 7771, 42, NULL)",
    )
    .await;

    // Mark's phone lists the sales reps (Assign a sales rep) ...
    let res = server
        .get("/v1/users/sales-reps")
        .add_header(HEADER, token(2))
        .await;
    res.assert_status_ok();
    let reps: Vec<Value> = res.json();
    assert!(reps.iter().any(|rep| rep["id"] == 100), "{reps:?}");

    // ... and gives Bob Unassigned (1001) to Rita.
    server
        .put("/v1/customers/1001/sales-rep")
        .add_header(HEADER, token(2))
        .json(&json!({ "sales_rep": 100 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let deal: i64 = sqlx::query_scalar(
        "SELECT CAST(id AS SIGNED) FROM deals
          WHERE customer_id = 1001 AND deleted_at IS NULL AND user_id = 100 ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    // Rita hears of it on her phone and in Telegram, both, in the words a
    // Telegram assignment uses. Mark assigned it: his phone is not told.
    assert_eq!(
        pushes(&stubs, "lead"),
        [(
            "push-1".to_string(),
            json!({ "granite": "1", "type": "lead", "kind": "assigned", "customerId": "1001",
                    "dealId": deal.to_string() })
        )]
    );
    let sent = chats.lock().unwrap().clone();
    assert_eq!(sent.len(), 3, "{sent:?}");
    let to_rita: Vec<_> = sent
        .iter()
        .filter(|(_, body)| body["chat_id"] == 5551)
        .collect();
    assert_eq!(to_rita.len(), 1, "{sent:?}");
    assert_eq!(to_rita[0].0, "bot111:leads/sendMessage");
    assert_eq!(
        to_rita[0].1["text"],
        format!(
            "You were assigned a lead. Click here: \nhttps://crm.test/employee/deals/edit/{deal}/project"
        )
    );
    // Mark's Telegram messages say who has it, as after pressing Rita's button.
    let edited = sent
        .iter()
        .find(|(path, _)| path == "bot111:leads/editMessageText")
        .expect("the kept text is edited");
    assert_eq!(edited.1["chat_id"], 7771);
    assert_eq!(edited.1["message_id"], 41);
    assert_eq!(
        edited.1["text"],
        "New lead: Bob Unassigned\nChoose a salesperson.\n\nLead assigned to Rita Rep"
    );
    let relabeled = sent
        .iter()
        .find(|(path, _)| path == "bot111:leads/editMessageReplyMarkup")
        .expect("the older message's buttons are replaced");
    assert_eq!(relabeled.1["message_id"], 42);
    assert_eq!(
        relabeled.1["reply_markup"]["inline_keyboard"][0][0]["text"],
        "Lead assigned to Rita Rep"
    );

    // The same rep again tells nobody again.
    server
        .put("/v1/customers/1001/sales-rep")
        .add_header(HEADER, token(2))
        .json(&json!({ "sales_rep": 100 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(pushes(&stubs, "lead").len(), 1);
    assert_eq!(chats.lock().unwrap().len(), 3);

    // A rep who takes a lead themselves is not told; Mark's phone and
    // Telegram messages are.
    server
        .put("/v1/customers/1001/sales-rep")
        .add_header(HEADER, token(2))
        .json(&json!({ "sales_rep": null }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .put("/v1/customers/1001/sales-rep")
        .add_header(HEADER, token(1))
        .json(&json!({ "sales_rep": 100 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let lead_pushes = pushes(&stubs, "lead");
    assert_eq!(lead_pushes.len(), 2, "{lead_pushes:?}");
    assert_eq!(lead_pushes[1].0, "push-2");
    assert_eq!(lead_pushes[1].1["kind"], "taken");
    assert_eq!(lead_pushes[1].1.get("assign"), None);
    let sent = chats.lock().unwrap().clone();
    assert_eq!(sent.len(), 5, "{sent:?}");
    assert_eq!(
        sent.iter()
            .filter(|(_, body)| body["chat_id"] == 5551)
            .count(),
        1
    );

    // The phone still may not edit or delete the customer.
    server
        .delete("/v1/customers/1001")
        .add_header(HEADER, token(2))
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn a_manager_marks_a_lead_invalid_from_the_phone(pool: MySqlPool) {
    let (server, _) = phone_server(pool.clone()).await;
    add_phone(&pool, 1, 100, 100).await;
    add_phone(&pool, 2, 101, 100).await;
    let mark = |n: u32| {
        server
            .put("/v1/customers/1001/invalid")
            .add_header(HEADER, token(n))
            .json(&json!({ "reason": "Looking for unrelated service" }))
    };

    // Rita, a rep, may not (as on the web); Mark, the sales manager, may.
    mark(1).await.assert_status(StatusCode::FORBIDDEN);
    mark(2).await.assert_status(StatusCode::NO_CONTENT);
    let reason: Option<String> =
        sqlx::query_scalar("SELECT invalid_lead FROM customers WHERE id = 1001")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(reason.as_deref(), Some("Looking for unrelated service"));
    let live: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM deals WHERE customer_id = 1001 AND deleted_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(live, 0);
}

#[test_db::test(seed)]
async fn a_scheduled_text_that_went_out_is_pushed_to_the_senders_app(pool: MySqlPool) {
    let (server, stubs) = setup(&pool).await;
    // Rita (100, phone 1) scheduled a text to a number with no customer (no deal): due now.
    sql(
        &pool,
        "INSERT INTO delayed_sms (company_id, user_id, customer_id, phone_digits, provider, send_at,
            cancel_if_customer_messages, baseline_at, status, payload_json)
         VALUES (100, 100, NULL, '3175559999', 'telnyx', UTC_TIMESTAMP() - INTERVAL 1 MINUTE, 0,
            UTC_TIMESTAMP() - INTERVAL 1 HOUR, 'pending', '{\"text\":\"See you Monday\"}')",
    )
    .await;
    job(&server, "/v1/jobs/sms-followups", json!({})).await;
    let status: String = sqlx::query_scalar("SELECT status FROM delayed_sms LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let kind = if status == "sent" {
        "delayed_message_sent"
    } else {
        "delayed_message_failed"
    };
    // Sent or not, her phone hears it, and the bell's notification opens for the app.
    let id: i64 = sqlx::query_scalar(
        "SELECT CAST(id AS SIGNED) FROM notifications WHERE user_id = 100 AND notification_type = ?",
    )
    .bind(kind)
    .fetch_one(&pool)
    .await
    .unwrap();
    let pushed = pushes(&stubs, "notification");
    assert_eq!(pushed.len(), 1, "{pushed:?}");
    assert_eq!(pushed[0].0, "push-1");
    assert_eq!(pushed[0].1["notificationId"], id.to_string());
    let heading = if status == "sent" {
        "Scheduled Message Sent"
    } else {
        "Scheduled Message Failed"
    };
    assert_eq!(pushed[0].1["heading"], heading);
    server
        .get(&format!("/v1/notifications/{id}"))
        .add_header(phone_common::HEADER, phone_common::token(1))
        .await
        .assert_status_ok();
}

#[test_db::test(seed)]
async fn a_customer_moved_to_another_rep_on_its_edit_page_says_reassigned(pool: MySqlPool) {
    use crate::common::{WithSession, sessions};
    use api::telegram::TelegramBots;
    let (telegram, chats) = telegram_stub().await;
    let (server, _stubs) = phone_common::phone_server_with(pool.clone(), |state| {
        state.with_telegram(TelegramBots::new(
            Some("111:leads"),
            Some("222:notes"),
            &telegram,
        ))
    })
    .await;
    sql(&pool, "UPDATE users SET telegram_id = 5557 WHERE id = 107").await;

    // Mark moves Alice Lead from Rita to Dina on Alice's Edit page.
    server
        .put("/v1/customers/1000")
        .with_session(sessions::MANAGER)
        .json(&json!({ "name": "Alice Lead", "source": "leads", "sales_rep": 107 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let deal: i64 = sqlx::query_scalar(
        "SELECT CAST(id AS SIGNED) FROM deals
          WHERE customer_id = 1000 AND deleted_at IS NULL AND user_id = 107 ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let to_dina = |chats: &std::sync::Mutex<Vec<(String, Value)>>| -> Vec<Value> {
        chats
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, body)| path == "bot111:leads/sendMessage" && body["chat_id"] == 5557)
            .map(|(_, body)| body["text"].clone())
            .collect()
    };
    assert_eq!(
        to_dina(&chats),
        [json!(format!(
            "You were reassigned the customer Alice Lead. Click here: \nhttps://crm.test/employee/deals/edit/{deal}/project"
        ))]
    );

    // A customer with no rep yet is still "assigned a lead".
    server
        .put("/v1/customers/1001")
        .with_session(sessions::MANAGER)
        .json(&json!({ "name": "Bob Unassigned", "source": "check-in", "sales_rep": 107 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let texts = to_dina(&chats);
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert!(
        texts[1]
            .as_str()
            .unwrap()
            .starts_with("You were assigned a lead. Click here: \n"),
        "{texts:?}"
    );
}
