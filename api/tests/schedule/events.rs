use crate::common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

const SETUP: &str = "
INSERT INTO calendars (id, company_id, name, color, slug) VALUES
  (8800, 100, 'Estimates', 'blue', 'estimates'), (8801, 100, 'Crew', 'teal', NULL),
  (8802, 101, 'Theirs', 'gray', NULL);
";

fn body(calendar: i64, start: &str, end: &str) -> Value {
    json!({
        "title": "Measure kitchen", "calendar_id": calendar,
        "start_date": start, "end_date": end, "location": "1 Main St",
    })
}

async fn history(pool: &MySqlPool, event: i64) -> Vec<String> {
    sqlx::query_scalar("SELECT message FROM event_history WHERE event_id = ? ORDER BY id")
        .bind(event)
        .fetch_all(pool)
        .await
        .unwrap()
}

#[test_db::test(seed)]
async fn events_are_added_edited_and_deleted(pool: MySqlPool) {
    sqlx::raw_sql(SETUP).execute(&pool).await.unwrap();
    let server = server(pool.clone());
    let create = |body: Value| {
        let request = server
            .post("/v1/events")
            .with_session(sessions::REP)
            .json(&body);
        async move { request.await }
    };
    for (bad, message) in [
        (
            json!({ "title": "x", "start_date": "2026-11-02 15:00:00", "end_date": "2026-11-02 16:00:00" }),
            "Calendar is required",
        ),
        (
            body(8801, "2026-11-02 15:00:00", "2026-11-02 14:00:00"),
            "End date must be on or after start date",
        ),
        (
            body(8802, "2026-11-02 15:00:00", "2026-11-02 16:00:00"),
            "Calendar not found",
        ),
        (
            body(8801, "soon", "2026-11-02 16:00:00"),
            "Invalid start date",
        ),
    ] {
        let res = create(bad).await;
        res.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(res.json::<Value>()["error"], message);
    }
    let res = create(body(8801, "2026-11-02T15:00", "2026-11-02 16:00:00")).await;
    res.assert_status(StatusCode::CREATED);
    let id = res.json::<Value>()["id"].as_i64().unwrap();
    let (start, color, created_by): (String, String, i32) = sqlx::query_as(
        "SELECT CAST(start_date AS CHAR), color, created_user_id FROM events WHERE id = ?",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (start.as_str(), color.as_str(), created_by),
        ("2026-11-02 15:00:00", "blue", 100)
    );
    assert_eq!(history(&pool, id).await, ["Event created"]);

    let mut edit = body(8801, "2026-11-03 15:00:00", "2026-11-03 16:00:00");
    edit["title"] = json!("Measure bath");
    edit["color"] = json!("red");
    edit["qbo_invoice"] = json!({ "id": "1042", "number": "INV-1042" });
    server
        .put(&format!("/v1/events/{id}"))
        .with_session(sessions::REP)
        .json(&edit)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        history(&pool, id).await,
        [
            "Event created",
            "Date updated",
            "Label updated",
            "Title changed to \"Measure bath\""
        ]
    );
    let invoice: String =
        sqlx::query_scalar("SELECT qbo_invoice_number FROM event_qbo_invoices WHERE event_id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(invoice, "INV-1042");

    // Another company's user cannot see it.
    server
        .put(&format!("/v1/events/{id}"))
        .with_session(sessions::OTHER_COMPANY_REP)
        .json(&edit)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .delete(&format!("/v1/events/{id}"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .delete(&format!("/v1/events/{id}"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn a_new_template_goes_to_the_only_template_maker(pool: MySqlPool) {
    sqlx::raw_sql(SETUP).execute(&pool).await.unwrap();
    sqlx::raw_sql(
        "INSERT INTO calendars (id, company_id, name, color, slug) VALUES
           (8803, 100, 'Templates', 'green', 'templates');
         INSERT INTO users_positions (user_id, position_id, company_id) VALUES
           (104, 11, 100), (105, 11, 100);",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool.clone());
    let assignee = |body: Value| {
        let request = server
            .post("/v1/events")
            .with_session(sessions::REP)
            .json(&body);
        let pool = pool.clone();
        async move {
            let res = request.await;
            res.assert_status(StatusCode::CREATED);
            let id = res.json::<Value>()["id"].as_i64().unwrap();
            sqlx::query_scalar::<_, Option<i32>>("SELECT assigned_user_id FROM events WHERE id = ?")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    let template = body(8803, "2026-11-02 15:00:00", "2026-11-02 16:00:00");
    // 105 is deleted, so 104 is the only one.
    assert_eq!(assignee(template.clone()).await, Some(104));
    // A pick wins; other calendars get no one.
    let mut picked = template.clone();
    picked["assigned_user_id"] = json!(101);
    assert_eq!(assignee(picked).await, Some(101));
    assert_eq!(
        assignee(body(8800, "2026-11-02 15:00:00", "2026-11-02 16:00:00")).await,
        None
    );
    // Two template makers: no one is guessed.
    sqlx::query(
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (101, 11, 100)",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(assignee(template).await, None);
}

#[test_db::test(seed)]
async fn events_link_a_customer_apart_from_their_title(pool: MySqlPool) {
    sqlx::raw_sql(SETUP).execute(&pool).await.unwrap();
    let server = server(pool.clone());
    let mut event = body(8801, "2026-11-02 15:00:00", "2026-11-02 16:00:00");
    event["title"] = json!("Alice's kitchen");
    event["customer_id"] = json!(2000);
    let res = server
        .post("/v1/events")
        .with_session(sessions::REP)
        .json(&event)
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(res.json::<Value>()["error"], "Customer not found");
    event["customer_id"] = json!(1000);
    let id = server
        .post("/v1/events")
        .with_session(sessions::REP)
        .json(&event)
        .await
        .json::<Value>()["id"]
        .as_i64()
        .unwrap();
    let customer = || async {
        let (title, customer): (String, Option<i32>) =
            sqlx::query_as("SELECT title, customer_id FROM events WHERE id = ?")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        (title, customer)
    };
    assert_eq!(customer().await, ("Alice's kitchen".into(), Some(1000)));
    let listed = server
        .get("/v1/schedule/events?start=2026-11-01T00:00:00Z&end=2026-11-30T00:00:00Z")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(listed["events"][0]["title"], "Alice's kitchen");
    assert_eq!(listed["events"][0]["customer_id"], 1000);
    assert_eq!(listed["events"][0]["customer_name"], "Alice Lead");

    // Left out, the customer stays; null unlinks them.
    let save = |body: Value| {
        let request = server
            .put(&format!("/v1/events/{id}"))
            .with_session(sessions::REP)
            .json(&body);
        async move {
            request.await.assert_status(StatusCode::NO_CONTENT);
        }
    };
    event.as_object_mut().unwrap().remove("customer_id");
    save(event.clone()).await;
    assert_eq!(customer().await.1, Some(1000));
    event["customer_id"] = json!(1004);
    save(event.clone()).await;
    assert_eq!(customer().await.1, Some(1004));
    event["customer_id"] = Value::Null;
    save(event).await;
    assert_eq!(customer().await, ("Alice's kitchen".into(), None));
    assert_eq!(
        history(&pool, id).await,
        ["Event created", "Customer changed", "Customer removed"]
    );
}

#[test_db::test(seed)]
async fn events_keep_their_own_phone_and_email(pool: MySqlPool) {
    sqlx::raw_sql(SETUP).execute(&pool).await.unwrap();
    let server = server(pool.clone());
    let mut event = body(8801, "2026-11-02 15:00:00", "2026-11-02 16:00:00");
    event["customer_id"] = json!(1000);
    event["contact_phone"] = json!(" (317) 555-0199 ");
    event["contact_email"] = json!("wife@example.com");
    let id = server
        .post("/v1/events")
        .with_session(sessions::REP)
        .json(&event)
        .await
        .json::<Value>()["id"]
        .as_i64()
        .unwrap();
    let contact = || async {
        let row: (Option<String>, Option<String>) =
            sqlx::query_as("SELECT contact_phone, contact_email FROM events WHERE id = ?")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        row
    };
    assert_eq!(
        contact().await,
        (
            Some("(317) 555-0199".into()),
            Some("wife@example.com".into())
        )
    );
    let listed = server
        .get("/v1/schedule/events?start=2026-11-01T00:00:00Z&end=2026-11-30T00:00:00Z")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(listed["events"][0]["contact_phone"], "(317) 555-0199");
    assert_eq!(listed["events"][0]["contact_email"], "wife@example.com");
    let customer_phone: Option<String> =
        sqlx::query_scalar("SELECT phone FROM customers WHERE id = 1000")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_ne!(customer_phone.as_deref(), Some("(317) 555-0199"));

    // Left out, they stay; empty goes back to the customer's.
    let save = |body: Value| {
        let request = server
            .put(&format!("/v1/events/{id}"))
            .with_session(sessions::REP)
            .json(&body);
        async move {
            request.await.assert_status(StatusCode::NO_CONTENT);
        }
    };
    let object = event.as_object_mut().unwrap();
    object.remove("contact_phone");
    object.remove("contact_email");
    save(event.clone()).await;
    assert_eq!(contact().await.1.as_deref(), Some("wife@example.com"));
    event["contact_phone"] = json!("");
    event["contact_email"] = Value::Null;
    save(event).await;
    assert_eq!(contact().await, (None, None));
}

#[test_db::test(seed)]
async fn a_copy_takes_comments_files_and_chosen_attachments(pool: MySqlPool) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "{SETUP}
         INSERT INTO events (id, title, start_date, end_date, created_user_id, calendar_id) VALUES
           (8900, 'Source', '2026-11-02 15:00:00', '2026-11-02 16:00:00', 101, 8801);
         INSERT INTO event_attachments (id, event_id, company_id, url, name, kind) VALUES
           (8910, 8900, 100, 'https://f/a.pdf', 'a.pdf', 'file'), (8911, 8900, 100, 'https://f/b.pdf', 'b.pdf', 'file');
         INSERT INTO event_comments (event_id, company_id, content, created_by) VALUES (8900, 100, 'Bring samples', 'Mark');
         INSERT INTO event_images (event_id, company_id, url) VALUES (8900, 100, 'https://f/p.jpg');"
    )))
    .execute(&pool)
    .await
    .unwrap();
    let mut copy = body(8801, "2026-11-09 15:00:00", "2026-11-09 16:00:00");
    copy["copy_from_event_id"] = json!(8900);
    copy["copy_attachment_ids"] = json!([8911]);
    let res = server(pool.clone())
        .post("/v1/events")
        .with_session(sessions::REP)
        .json(&copy)
        .await;
    res.assert_status(StatusCode::CREATED);
    let id = res.json::<Value>()["id"].as_i64().unwrap();
    let counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM event_attachments WHERE event_id = ? AND name = 'b.pdf'),
                (SELECT COUNT(*) FROM event_comments WHERE event_id = ? AND created_by = 'Mark'),
                (SELECT COUNT(*) FROM event_images WHERE event_id = ?)",
    )
    .bind(id)
    .bind(id)
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(counts, (1, 1, 1));
    assert_eq!(
        history(&pool, id).await,
        ["Event created", "Copied 1 attachment", "Copied 1 comment"]
    );
}

#[test_db::test(seed)]
async fn moving_an_appointment_moves_its_reminders(pool: MySqlPool) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "{SETUP}
         INSERT INTO events (id, title, start_date, end_date, created_user_id, calendar_id) VALUES
           (8901, 'Estimate', '2099-11-02 15:00:00', '2099-11-02 16:00:00', 100, 8800);
         INSERT INTO appointment_reminders (event_id, company_id, customer_id, calendar_slug, reminder_kind, send_at, status)
           VALUES (8901, 100, 1000, 'estimates', '24h', '2099-11-01 15:00:00', 'pending');"
    )))
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool.clone());
    server
        .put("/v1/events/8901")
        .with_session(sessions::REP)
        .json(&body(8800, "2099-11-05 14:00:00", "2099-11-05 15:00:00"))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let customer: Vec<(String, String)> = sqlx::query_as(
        "SELECT reminder_kind, CAST(send_at AS CHAR) FROM appointment_reminders WHERE event_id = 8901 ORDER BY reminder_kind",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        customer,
        [
            ("1h".into(), "2099-11-05 13:00:00".into()),
            ("24h".into(), "2099-11-04 14:00:00".into())
        ]
    );
    // Alice's sales rep (Rita) is reminded at 5 PM Indianapolis (EST, UTC-5) the day before.
    let rep: Vec<(String, i32, String)> = sqlx::query_as(
        "SELECT reminder_kind, user_id, CAST(send_at AS CHAR) FROM appointment_rep_reminders WHERE event_id = 8901 ORDER BY reminder_kind",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rep,
        [
            ("1h".into(), 100, "2099-11-05 13:00:00".into()),
            ("day_before_5pm".into(), 100, "2099-11-04 22:00:00".into())
        ]
    );
    server
        .delete("/v1/events/8901")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let pending: i64 = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM appointment_reminders WHERE event_id = 8901 AND status = 'pending')
              + (SELECT COUNT(*) FROM appointment_rep_reminders WHERE event_id = 8901 AND status = 'pending')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(pending, 0);
}

const EVENT: &str = "
INSERT INTO calendars (id, company_id, name, color) VALUES (8803, 100, 'Crew', 'teal');
INSERT INTO events (id, title, location, start_date, end_date, created_user_id, assigned_user_id, calendar_id, customer_id) VALUES
  (8950, 'Kitchen install', '9 Elm St', '2026-11-02 15:00:00', '2026-11-02 16:00:00', 101, 100, 8803, 1000),
  (8951, 'Their event', NULL, '2026-11-02 15:00:00', '2026-11-02 16:00:00', 106, NULL, NULL, NULL);
INSERT INTO event_comments (id, event_id, company_id, content, created_by) VALUES
  (8960, 8950, 100, 'Mark wrote this', 'Mark Manager');
";

#[test_db::test(seed)]
async fn comments_are_edited_only_by_their_author(pool: MySqlPool) {
    sqlx::raw_sql(EVENT).execute(&pool).await.unwrap();
    let server = server(pool.clone());
    server
        .post("/v1/events/8950/comments")
        .with_session(sessions::REP)
        .json(&json!({ "content": "  On my way " }))
        .await
        .assert_status(StatusCode::CREATED);
    server
        .post("/v1/events/8950/comments")
        .with_session(sessions::REP)
        .json(&json!({ "content": " " }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    let details: Value = server
        .get("/v1/events/8950/details")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(details["creator"]["id"], 101);
    assert_eq!(details["assignee"]["id"], 100);
    assert_eq!(details["viewer_name"], "Rita Rep");
    let mine = details["comments"][1]["id"].as_i64().unwrap();
    assert_eq!(details["comments"][1]["content"], "On my way");

    server
        .put("/v1/events/8950/comments/8960")
        .with_session(sessions::REP)
        .json(&json!({ "content": "hijack" }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .put(&format!("/v1/events/8950/comments/{mine}"))
        .with_session(sessions::REP)
        .json(&json!({ "content": "Running late" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .delete("/v1/events/8950/comments/8960")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .delete(&format!("/v1/events/8950/comments/{mine}"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .get("/v1/events/8951/details")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn files_are_attached_from_the_events_folder(pool: MySqlPool) {
    sqlx::raw_sql(EVENT).execute(&pool).await.unwrap();
    let (server, storage) = common::server_with_storage(pool.clone());
    let mut urls = Vec::new();
    for (name, content_type, bytes) in [
        ("site.png", "image/png", common::png(8, 8)),
        ("Quote.pdf", "application/pdf", b"%PDF-1.4".to_vec()),
    ] {
        let res = server
            .post("/v1/uploads")
            .with_session(sessions::REP)
            .json(&json!({ "folder": "events", "file_name": name, "content_type": content_type }))
            .await;
        res.assert_status(StatusCode::CREATED);
        let url = res.json::<Value>()["url"].as_str().unwrap().to_string();
        storage
            .put(&storage.key_for_url(&url).unwrap(), bytes, content_type)
            .await
            .unwrap();
        urls.push((url, name));
    }
    server
        .post("/v1/events/8950/attachments")
        .with_session(sessions::REP)
        .json(&json!({ "files": urls.iter().map(|(url, name)| json!({ "url": url, "name": name })).collect::<Vec<_>>() }))
        .await
        .assert_status(StatusCode::CREATED);
    server
        .post("/v1/events/8950/photos")
        .with_session(sessions::REP)
        .json(&json!({ "urls": [urls[0].0] }))
        .await
        .assert_status(StatusCode::CREATED);
    server
        .post("/v1/events/8950/photos")
        .with_session(sessions::REP)
        .json(&json!({ "urls": ["https://elsewhere.test/x.png"] }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    let details: Value = server
        .get("/v1/events/8950/details")
        .with_session(sessions::REP)
        .await
        .json();
    let kinds: Vec<(&str, &str)> = details["attachments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| (a["name"].as_str().unwrap(), a["kind"].as_str().unwrap()))
        .collect();
    assert_eq!(kinds, [("site.png", "image"), ("Quote.pdf", "document")]);
    assert_eq!(details["photos"][0]["source"], "photo");
    let photo = details["photos"][0]["id"].as_i64().unwrap();
    let attachment = details["attachments"][0]["id"].as_i64().unwrap();

    // The image is still used by the photo, so removing the attachment keeps the file.
    server
        .delete(&format!("/v1/events/8950/attachments/{attachment}"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let key = storage.key_for_url(&urls[0].0).unwrap();
    assert!(storage.get(&key).await.unwrap().is_some());
    server
        .delete(&format!("/v1/events/8950/photos/{photo}"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert!(storage.get(&key).await.unwrap().is_none());
    assert_eq!(
        history(&pool, 8950).await,
        ["Attachment added", "Attachment removed"]
    );
}

#[test_db::test(seed)]
async fn search_finds_titles_customers_and_places(pool: MySqlPool) {
    sqlx::raw_sql(EVENT).execute(&pool).await.unwrap();
    let server = server(pool);
    let search = |q: &str| {
        let request = server
            .get("/v1/events/search")
            .add_query_param("q", q)
            .with_session(sessions::REP);
        async move { request.await.json::<Value>() }
    };
    let alice = search("Alice").await;
    assert_eq!(alice[0]["id"], 8950);
    assert_eq!(alice[0]["customer_id"], 1000);
    assert_eq!(alice[0]["customer_name"], "Alice Lead");
    assert_eq!(alice[0]["start_date"], "2026-11-02T15:00:00Z");
    assert_eq!(search("Kitchen").await[0]["id"], 8950);
    assert_eq!(search("Elm").await[0]["id"], 8950);
    assert_eq!(search("Their").await, json!([]));
    assert_eq!(search(" ").await, json!([]));
}
