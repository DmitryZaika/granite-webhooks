//! The Granite Phone app's Emails and Schedule screens call the CRM's own
//! routes (`/v1/emails`, `/v1/schedule`, `/v1/events`) with its device token
//! (`X-Granite-Device`) instead of the cookie (`auth::DEVICE_ROUTES`), and so
//! does its schedule-only app for installers and template makers
//! (`/v1/me/schedule`).
//!
//! Fixture phones use n = 301..=310; events 9750+.

use crate::common;
use crate::phone_common;

use axum::http::StatusCode;
use common::{WithSession, sessions};
use phone_common::{
    HEADER, add_phone, add_session, enable_calling, phone_server, phone_server_with,
    revoke_session, sql, token,
};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const UNREAD: &str = "/v1/emails/unread";
const EVENTS: &str = "/v1/schedule/events?start=2026-10-01T00:00:00Z&end=2026-11-01T00:00:00Z";
const MINE: &str = "/v1/me/schedule?start=2026-10-01T00:00:00Z&end=2026-11-01T00:00:00Z";
const PASSWORD: &str = "granite-local";

/// The `portal` of the bootstrap of phone `n` (the app's schedule-only switch).
async fn portal(server: &axum_test::TestServer, n: u32) -> Value {
    let response = server
        .get("/v1/phone/bootstrap")
        .add_header(HEADER, token(n))
        .await;
    response.assert_status_ok();
    response.json::<Value>()["user"]["portal"].clone()
}

#[test_db::test(seed)]
async fn installers_and_template_makers_get_the_schedule_only_app(pool: MySqlPool) {
    // Rita (user 100, company 100) also installs; Olga (106, company 101)
    // makes templates; Sam (103) is a super admin who also installs. Nobody
    // has a phone line or a Telnyx number.
    sql(
        &pool,
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES
           (100, 6, 100), (106, 11, 101), (103, 6, 100)",
    )
    .await;
    sql(
        &pool,
        "UPDATE company SET address = ' 1 Stone Way, Indianapolis ' WHERE id = 100",
    )
    .await;
    sql(
        &pool,
        "INSERT INTO events (id, title, location, start_date, end_date, created_user_id, assigned_user_id) VALUES
           (9750, 'Rita''s job', '5 Elm St', '2026-10-12 14:00:00', '2026-10-12 16:00:00', 101, 100),
           (9751, 'Mark''s job', '6 Oak St', '2026-10-12 14:00:00', '2026-10-12 16:00:00', 101, 101),
           (9752, 'Unassigned', NULL, '2026-10-13 14:00:00', '2026-10-13 16:00:00', 101, NULL)",
    )
    .await;
    let (server, _) = phone_server(pool.clone()).await;

    // An installer signs a phone in with the CRM password like anyone else;
    // the portal tells the app to show only the schedule.
    let signed_in = server
        .post("/v1/auth/device-sessions")
        .json(&json!({ "email": "rep@acme.test", "password": PASSWORD,
                       "deviceName": "Rita's Pixel 9", "platform": "android",
                       "appVersion": "1.4.0" }))
        .await;
    signed_in.assert_status(StatusCode::CREATED);
    assert_eq!(signed_in.json::<Value>()["user"]["portal"], "installer");

    // The bootstrap loads without a phone line, number or registered phone.
    add_session(&pool, 304, 100, 100).await;
    let bootstrap = server
        .get("/v1/phone/bootstrap")
        .add_header(HEADER, token(304))
        .await;
    bootstrap.assert_status_ok();
    let bootstrap: Value = bootstrap.json();
    assert_eq!(bootstrap["user"]["portal"], "installer");
    assert_eq!(bootstrap["device"], Value::Null);
    assert_eq!(bootstrap["callerIds"], json!([]));
    assert_eq!(bootstrap["callingEnabled"], false);

    // Her own events only, the same as her CRM session gets.
    let by_phone = server.get(MINE).add_header(HEADER, token(304)).await;
    by_phone.assert_status_ok();
    let by_phone: Value = by_phone.json();
    let ids: Vec<i64> = by_phone["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, [9750]);
    let by_cookie: Value = server.get(MINE).with_session(sessions::REP).await.json();
    assert_eq!(by_phone, by_cookie);
    // Another person's schedule stays an admin's, by phone too.
    server
        .get(&format!("{MINE}&user_id=101"))
        .add_header(HEADER, token(304))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    // The whole-day route starts at the company address: the day map without
    // stops answers it and draws nothing.
    let home = server
        .get("/v1/schedule/day-map")
        .add_header(HEADER, token(304))
        .await;
    home.assert_status_ok();
    assert_eq!(
        home.json::<Value>(),
        json!({ "image": null, "home_address": "1 Stone Way, Indianapolis", "home": null,
                "stops": [], "route": null })
    );
    // A revoked phone reads no schedule.
    add_session(&pool, 305, 100, 100).await;
    revoke_session(&pool, 305).await;
    server
        .get(MINE)
        .add_header(HEADER, token(305))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    // The device token stops at /v1/me/schedule and the to-do list: the rest
    // of /v1/me needs the cookie.
    server
        .get("/v1/me")
        .add_header(HEADER, token(304))
        .await
        .assert_status(StatusCode::FORBIDDEN);

    // A template maker gets the template portal; a super admin never gets one,
    // and neither does a sales rep.
    add_session(&pool, 306, 106, 101).await;
    assert_eq!(portal(&server, 306).await, "template");
    add_session(&pool, 307, 103, 101).await;
    assert_eq!(portal(&server, 307).await, Value::Null);
    sql(
        &pool,
        "DELETE FROM users_positions WHERE user_id = 100 AND position_id = 6",
    )
    .await;
    assert_eq!(portal(&server, 304).await, Value::Null);
}

#[test_db::test(seed)]
async fn the_to_do_tab_keeps_the_list_with_its_device_token(pool: MySqlPool) {
    // Rita (user 100, company 100) signed in on her phone.
    add_session(&pool, 309, 100, 100).await;
    let (server, _) = phone_server(pool.clone()).await;

    let added = server
        .post("/v1/me/todos")
        .add_header(HEADER, token(309))
        .json(&json!({ "rich_text": "Call the fabricator" }))
        .await;
    added.assert_status(StatusCode::CREATED);
    let id = added.json::<Value>()["id"].as_i64().unwrap();

    // The phone and her CRM session read the same list.
    let by_phone = server
        .get("/v1/me/todos")
        .add_header(HEADER, token(309))
        .await;
    by_phone.assert_status_ok();
    let by_cookie = server.get("/v1/me/todos").with_session(sessions::REP).await;
    by_cookie.assert_status_ok();
    assert_eq!(by_phone.json::<Value>(), by_cookie.json::<Value>());
    assert!(
        by_phone.json::<Vec<Value>>().iter().any(|t| t["id"] == id),
        "the new to-do is listed"
    );

    server
        .put(&format!("/v1/me/todos/{id}/done"))
        .add_header(HEADER, token(309))
        .json(&json!({ "done": true }))
        .await
        .assert_status_success();
    server
        .delete("/v1/me/todos/done")
        .add_header(HEADER, token(309))
        .await
        .assert_status_success();

    // A revoked phone reads no list.
    revoke_session(&pool, 309).await;
    server
        .get("/v1/me/todos")
        .add_header(HEADER, token(309))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

#[test_db::test(seed)]
async fn the_app_reads_emails_and_the_schedule_with_its_device_token(pool: MySqlPool) {
    // Rita (user 100, company 100) signed in on her phone.
    add_session(&pool, 301, 100, 100).await;
    // A job waiting in the Unscheduled list.
    sql(
        &pool,
        "INSERT INTO events (id, title, start_date, end_date, created_user_id, status, unscheduled)
           VALUES (9790, 'Waiting', '2026-10-02 14:00:00', '2026-10-02 14:00:00', 100, 'called', 1)",
    )
    .await;
    let (server, _) = phone_server(pool.clone()).await;

    // The same answers as her CRM session gets, the Unscheduled list too.
    for path in [UNREAD, EVENTS, "/v1/schedule/unscheduled"] {
        let by_phone = server.get(path).add_header(HEADER, token(301)).await;
        by_phone.assert_status_ok();
        let by_cookie = server.get(path).with_session(sessions::REP).await;
        by_cookie.assert_status_ok();
        assert_eq!(
            by_phone.json::<Value>(),
            by_cookie.json::<Value>(),
            "{path}"
        );
    }
    let waiting: Value = server
        .get("/v1/schedule/unscheduled")
        .add_header(HEADER, token(301))
        .await
        .json();
    assert_eq!(waiting[0]["id"], 9790);
    assert_eq!(waiting[0]["status"], "called");

    // Nothing without a token or cookie; a bad or revoked token is out even
    // with a cookie (the header decides alone). One long unused is still in.
    server
        .get(UNREAD)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    server
        .get(UNREAD)
        .add_header(HEADER, "not-a-token")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    add_session(&pool, 302, 100, 100).await;
    revoke_session(&pool, 302).await;
    server
        .get(EVENTS)
        .add_header(HEADER, token(302))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    add_session(&pool, 303, 100, 100).await;
    sql(
        &pool,
        "UPDATE device_sessions SET last_seen_at = UTC_TIMESTAMP() - INTERVAL 31 DAY,
                created_at = UTC_TIMESTAMP() - INTERVAL 40 DAY
          WHERE id = 950303",
    )
    .await;
    server
        .get(UNREAD)
        .add_header(HEADER, token(303))
        .await
        .assert_status_ok();

    // Elsewhere the cookie routes still need the cookie, even right next to
    // the prefixes (`/v1/email-attachments` is not under `/v1/emails`).
    for path in ["/v1/company/tax-rate", "/v1/email-attachments/1/preview"] {
        server
            .get(path)
            .add_header(HEADER, token(301))
            .await
            .assert_status(StatusCode::FORBIDDEN);
    }
    // The app's own routes took the token everywhere before, and still do.
    server
        .get("/v1/me/devices")
        .add_header(HEADER, token(301))
        .await
        .assert_status_ok();
}

#[test_db::test(seed)]
async fn the_composer_reads_its_lists_and_changes_only_the_templates(pool: MySqlPool) {
    // Rita (user 100, company 100) signed in on her phone. Her company texts
    // through Telnyx: without a texting provider there are no SMS templates.
    add_session(&pool, 305, 100, 100).await;
    enable_calling(&pool, 100, "messaging_profile_id = 'profile-100'").await;
    let (server, _) = phone_server(pool.clone()).await;

    // From Stones, From Images, From Documents, Use Template, the email
    // recipient suggestions, the schedule's calendars and the More screens
    // (deals, customers, stones, suppliers, sales) read with the device
    // token, as with her cookie.
    for path in [
        "/v1/stones/attachable",
        "/v1/images",
        "/v1/documents",
        "/v1/email-templates",
        "/v1/sms-templates",
        "/v1/calendars",
        "/v1/customers/email-recipients?term=a&include_users=true",
        // The More screens.
        "/v1/deal-groups",
        "/v1/deal-lists?group_id=100",
        "/v1/deals?group_id=100",
        "/v1/customers/page?tab=all",
        "/v1/customers/1000",
        "/v1/stones",
        "/v1/suppliers/directory",
        "/v1/transactions",
    ] {
        let by_phone = server.get(path).add_header(HEADER, token(305)).await;
        by_phone.assert_status_ok();
        let by_cookie = server.get(path).with_session(sessions::REP).await;
        by_cookie.assert_status_ok();
        assert_eq!(
            by_phone.json::<Value>(),
            by_cookie.json::<Value>(),
            "{path}"
        );
    }

    // Those lists are read-only to the phone but for the templates, which Use
    // Template adds, edits and deletes: other writes need the CRM session.
    server
        .post("/v1/email-templates")
        .add_header(HEADER, token(305))
        .json(&json!({ "template_name": "x", "template_subject": "x", "template_body": "x" }))
        .await
        .assert_status(StatusCode::CREATED);
    server
        .delete("/v1/calendars/1")
        .add_header(HEADER, token(305))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .delete("/v1/customers/1000")
        .add_header(HEADER, token(305))
        .await
        .assert_status(StatusCode::FORBIDDEN);

    // An email attachment's upload URL comes with the token (the send route
    // asks for its `token`).
    let upload = server
        .post("/v1/uploads")
        .add_header(HEADER, token(305))
        .json(&json!({ "folder": "emails", "file_name": "kitchen.jpg",
                       "content_type": "image/jpeg" }))
        .await;
    assert_ne!(upload.status_code(), StatusCode::UNAUTHORIZED);
    assert_ne!(upload.status_code(), StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn the_to_do_tab_reads_and_closes_ai_promises_with_its_device_token(pool: MySqlPool) {
    // Rita (user 100) has AI promises and one open promise; her phone is signed in.
    sql(&pool, "UPDATE users SET ai_promises = 1 WHERE id = 100").await;
    let id: u64 = sqlx::query(
        "INSERT INTO ai_promises
                (company_id, user_id, scan_id, source, kind, promised_at,
                 customer_id, customer_digits, title, said, due_at)
         VALUES (100, 100, 1, 'call', 'promise', '2026-10-29 10:00:00',
                 1000, '3175550100', 'Send the price', '', '2026-10-30 15:00:00')",
    )
    .execute(&pool)
    .await
    .unwrap()
    .last_insert_id();
    add_session(&pool, 311, 100, 100).await;
    let (server, _) = phone_server(pool.clone()).await;

    // The list, as her CRM session reads it.
    let by_phone = server
        .get("/v1/ai/promises")
        .add_header(HEADER, token(311))
        .await;
    by_phone.assert_status_ok();
    let by_cookie = server
        .get("/v1/ai/promises")
        .with_session(sessions::REP)
        .await;
    assert_eq!(by_phone.json::<Value>(), by_cookie.json::<Value>());
    assert_eq!(
        by_phone.json::<Value>()["promises"][0]["title"],
        "Send the price"
    );

    // Done from the phone.
    server
        .patch(&format!("/v1/ai/promises/{id}"))
        .add_header(HEADER, token(311))
        .json(&json!({ "status": "done" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let status: String =
        sqlx::query_scalar("SELECT CAST(status AS CHAR) FROM ai_promises WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "done");
}

#[test_db::test(seed)]
async fn the_quote_screen_reads_and_saves_a_quote_with_its_device_token(pool: MySqlPool) {
    // Company 100 has a price list with Granite and a tax rate; Rita (user
    // 100) signed in on her phone.
    sql(
        &pool,
        "INSERT INTO quote_tax_rates (company_id, name, sequence, tax_pct) VALUES (100, 'Marion', 1, 7.25)",
    )
    .await;
    add_session(&pool, 312, 100, 100).await;
    let (server, stubs) = phone_server(pool.clone()).await;
    let list = server
        .post("/v1/quote-price-lists")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Retail" }))
        .await;
    list.assert_status(StatusCode::CREATED);
    let list_id = list.json::<Value>()["id"].as_i64().unwrap();
    let info = json!({
        "units": "in", "currency": "$", "lineItemPriceRounding": 2,
        "arrProducts": [{ "name": "Granite", "allowDiscount": 1,
                          "arrVariants": [{ "name": "Ubatuba", "price": 40 }] }],
        "arrOtherItems": [], "arrSinkModels": [],
    });
    sqlx::query(
        "UPDATE price_list_revisions r JOIN price_lists pl ON pl.current_revision_id = r.id
            SET r.info_json = ? WHERE pl.id = ?",
    )
    .bind(info.to_string())
    .bind(list_id)
    .execute(&pool)
    .await
    .unwrap();

    // New quote, from the phone.
    let created = server
        .post("/v1/quotes")
        .add_header(HEADER, token(312))
        .json(&json!({ "customer_id": 1000, "quote_name": "Kitchen", "price_list_id": list_id }))
        .await;
    created.assert_status(StatusCode::CREATED);
    let id = created.json::<Value>()["id"].as_i64().unwrap();
    let path = format!("/v1/quotes/{id}");

    // The quote and the editor's lookups read as with her cookie.
    for read in [
        path.clone(),
        "/v1/quote-price-lists".to_string(),
        "/v1/quote-tax-rates".to_string(),
        "/v1/quote-payment-terms".to_string(),
        "/v1/quote-forms?active=true".to_string(),
        "/v1/me/quote-email-templates".to_string(),
    ] {
        let by_phone = server.get(&read).add_header(HEADER, token(312)).await;
        by_phone.assert_status_ok();
        let by_cookie = server.get(&read).with_session(sessions::REP).await;
        assert_eq!(
            by_phone.json::<Value>(),
            by_cookie.json::<Value>(),
            "{read}"
        );
    }
    let quote = server
        .get(&path)
        .add_header(HEADER, token(312))
        .await
        .json::<Value>();
    assert_eq!(quote["quote"]["name"], "Kitchen");
    assert_eq!(quote["price_list"]["arrProducts"][0]["name"], "Granite");

    // Its info, then a saved revision with its notes.
    server
        .patch(&path)
        .add_header(HEADER, token(312))
        .json(&json!({ "quote_name": "Kitchen and bath", "expiration_date": "2026-12-31" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let doc = json!({
        "version": 2,
        "areas": [{ "id": "area-1", "name": "Kitchen", "quantity": 1 }],
        "colorOptions": [{ "id": "option-1",
                           "byArea": { "area-1": { "product": "Granite", "color": "Ubatuba" } } }],
        "items": [{ "id": "i1", "kind": "misc", "areaId": null, "name": "Sealer",
                    "quantity": 1, "price": 100 }],
    });
    let saved = server
        .put(&format!("{path}/drawing"))
        .add_header(HEADER, token(312))
        .json(&json!({ "doc": doc, "notes": "From the phone" }))
        .await;
    saved.assert_status_ok();
    assert_eq!(saved.json::<Value>()["revision"], 1);
    let quote = server
        .get(&path)
        .add_header(HEADER, token(312))
        .await
        .json::<Value>();
    assert_eq!(quote["quote"]["name"], "Kitchen and bath");
    assert_eq!(quote["quote"]["expiration_date"], "2026-12-31");
    assert_eq!(quote["revisions"][0]["notes"], "From the phone");
    assert_eq!(quote["doc"]["items"][0]["name"], "Sealer");

    // Emailed, sold and the sale undone.
    server
        .post(&format!("{path}/emails"))
        .add_header(HEADER, token(312))
        .json(
            &json!({ "to": ["alice@example.com"], "subject": "Your quote",
                       "body": "Here it is: [QuoteLink]" }),
        )
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(stubs.emails().len(), 1);
    server
        .post(&format!("{path}/sale"))
        .add_header(HEADER, token(312))
        .json(&json!({ "option": 0 }))
        .await
        .assert_status_ok();
    let status: String = sqlx::query_scalar("SELECT status FROM quotes WHERE id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "converted");
    server
        .delete(&format!("{path}/sale"))
        .add_header(HEADER, token(312))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    // The customer link, the price list's editor and the company's quote
    // setup still need the CRM session.
    for (method, route) in [
        ("POST", format!("{path}/portal-token")),
        ("GET", "/v1/quote-extras".to_string()),
        ("GET", format!("/v1/quote-price-lists/{list_id}")),
        ("POST", "/v1/quote-sale-events".to_string()),
        ("PUT", "/v1/company/auto-unscheduled-jobs".to_string()),
    ] {
        let request = match method {
            "POST" => server.post(&route).json(&json!({})),
            "PUT" => server.put(&route).json(&json!({})),
            _ => server.get(&route),
        };
        request
            .add_header(HEADER, token(312))
            .await
            .assert_status(StatusCode::FORBIDDEN);
    }
}

/// A Retail price list with Granite (Ubatuba, $40) in company 100.
async fn granite_price_list(server: &axum_test::TestServer, pool: &MySqlPool) -> i64 {
    let list = server
        .post("/v1/quote-price-lists")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Retail" }))
        .await;
    list.assert_status(StatusCode::CREATED);
    let list_id = list.json::<Value>()["id"].as_i64().unwrap();
    let info = json!({
        "units": "in", "currency": "$", "lineItemPriceRounding": 2,
        "arrProducts": [{ "name": "Granite", "allowDiscount": 1,
                          "arrVariants": [{ "name": "Ubatuba", "price": 40 }] }],
        "arrOtherItems": [], "arrSinkModels": [],
    });
    sqlx::query(
        "UPDATE price_list_revisions r JOIN price_lists pl ON pl.current_revision_id = r.id
            SET r.info_json = ? WHERE pl.id = ?",
    )
    .bind(info.to_string())
    .bind(list_id)
    .execute(pool)
    .await
    .unwrap();
    list_id
}

#[test_db::test(seed)]
async fn the_quote_screen_sells_copies_reprices_prints_and_deletes_with_its_device_token(
    pool: MySqlPool,
) {
    // Rita (user 100, company 100) on her phone; the company has a Plumbers
    // calendar with a "Plumbing" event template for Mark as Sold.
    add_session(&pool, 313, 100, 100).await;
    let (server, stubs) = phone_server(pool.clone()).await;
    let list_id = granite_price_list(&server, &pool).await;
    sql(
        &pool,
        "INSERT INTO calendars (company_id, name, color, slug) VALUES (100, 'Plumbers', 'green', NULL)",
    )
    .await;
    let plumbers: i32 = sqlx::query_scalar("SELECT id FROM calendars WHERE name = 'Plumbers'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let template = server
        .post("/v1/quote-sale-events")
        .with_session(sessions::ADMIN)
        .json(&json!({ "calendar_id": plumbers, "title_text": "Plumbing" }))
        .await;
    template.assert_status(StatusCode::CREATED);
    let template_id = template.json::<Value>()["id"].as_i64().unwrap();

    // Mark as Sold's and Print's lookups read as with her cookie.
    for read in [
        "/v1/company/features",
        "/v1/quote-sale-events",
        "/v1/places/check?address=1%20Main%20St%2C%20Carmel",
        // The lead card's miles to the customer.
        "/v1/company/drive-miles?address=Indianapolis%2046224",
    ] {
        let by_phone = server.get(read).add_header(HEADER, token(313)).await;
        by_phone.assert_status_ok();
        let by_cookie = server.get(read).with_session(sessions::REP).await;
        assert_eq!(
            by_phone.json::<Value>(),
            by_cookie.json::<Value>(),
            "{read}"
        );
    }
    let events = server
        .get("/v1/quote-sale-events")
        .add_header(HEADER, token(313))
        .await
        .json::<Value>();
    assert_eq!(events[0]["title_text"], "Plumbing");

    let created = server
        .post("/v1/quotes")
        .add_header(HEADER, token(313))
        .json(&json!({ "customer_id": 1000, "quote_name": "Kitchen", "price_list_id": list_id }))
        .await;
    created.assert_status(StatusCode::CREATED);
    let id = created.json::<Value>()["id"].as_i64().unwrap();
    let path = format!("/v1/quotes/{id}");
    let doc = json!({
        "version": 2,
        "areas": [{ "id": "area-1", "name": "Kitchen", "quantity": 1 }],
        "colorOptions": [{ "id": "option-1",
                           "byArea": { "area-1": { "product": "Granite", "color": "Ubatuba" } } }],
        "items": [{ "id": "i1", "kind": "misc", "areaId": null, "name": "Sealer",
                    "quantity": 1, "price": 100 }],
    });
    server
        .put(&format!("{path}/drawing"))
        .add_header(HEADER, token(313))
        .json(&json!({ "doc": doc, "notes": "First" }))
        .await
        .assert_status_ok();

    // Unsaved changes kept every 15 seconds, then discarded.
    server
        .put(&format!("{path}/autosave"))
        .add_header(HEADER, token(313))
        .json(&json!({ "doc": doc }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let quote = server
        .get(&path)
        .add_header(HEADER, token(313))
        .await
        .json::<Value>();
    assert_eq!(quote["autosave"]["doc"]["items"][0]["name"], "Sealer");
    server
        .delete(&format!("{path}/autosave"))
        .add_header(HEADER, token(313))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let quote = server
        .get(&path)
        .add_header(HEADER, token(313))
        .await
        .json::<Value>();
    assert_eq!(quote["autosave"], Value::Null);

    // Update prices, and a print recorded.
    server
        .post(&format!("{path}/prices"))
        .add_header(HEADER, token(313))
        .json(&json!({}))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .post(&format!("{path}/prints"))
        .add_header(HEADER, token(313))
        .json(&json!({ "form_label": "Quote (line items)" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    // Emailed, then its link switched off and on again.
    server
        .post(&format!("{path}/emails"))
        .add_header(HEADER, token(313))
        .json(
            &json!({ "to": ["alice@example.com"], "subject": "Your quote",
                       "body": "Here it is: [QuoteLink]" }),
        )
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(stubs.emails().len(), 1);
    let quote = server
        .get(&path)
        .add_header(HEADER, token(313))
        .await
        .json::<Value>();
    let email_id = quote["emails"][0]["id"].as_i64().unwrap();
    for active in [false, true] {
        server
            .patch(&format!("{path}/emails/{email_id}"))
            .add_header(HEADER, token(313))
            .json(&json!({ "active": active }))
            .await
            .assert_status(StatusCode::NO_CONTENT);
        let quote = server
            .get(&path)
            .add_header(HEADER, token(313))
            .await
            .json::<Value>();
        assert_eq!(quote["emails"][0]["deactivated_at"].is_null(), active);
    }

    // Sold with the template not ready, job notes and the Plumbing event.
    server
        .post(&format!("{path}/sale"))
        .add_header(HEADER, token(313))
        .json(&json!({
            "option": 0,
            "job_status": "not_ready",
            "template_notes": "Measure twice",
            "install_notes": "Back door",
            "full_height_backsplash": true,
            "extra_events": [{ "template_id": template_id, "notes": "Two sinks" }],
        }))
        .await
        .assert_status_ok();
    let extra: (String, Option<String>) = sqlx::query_as(
        "SELECT title, notes FROM events WHERE quote_id = ? AND calendar_id = ? AND deleted_date IS NULL",
    )
    .bind(id)
    .bind(plumbers)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        extra,
        (
            "Kitchen - Plumbing".to_string(),
            Some("Two sinks".to_string())
        )
    );

    // Copied, with its drawing, then the copy deleted.
    let copy = server
        .post(&format!("{path}/copies"))
        .add_header(HEADER, token(313))
        .json(&json!({ "quote_name": "Kitchen copy", "copy_address": true, "copy_drawing": true }))
        .await;
    copy.assert_status(StatusCode::CREATED);
    let copy_id = copy.json::<Value>()["id"].as_i64().unwrap();
    let copied = server
        .get(&format!("/v1/quotes/{copy_id}"))
        .add_header(HEADER, token(313))
        .await
        .json::<Value>();
    assert_eq!(copied["quote"]["name"], "Kitchen copy");
    assert_eq!(copied["doc"]["items"][0]["name"], "Sealer");
    server
        .delete(&format!("/v1/quotes/{copy_id}"))
        .add_header(HEADER, token(313))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let deleted: bool =
        sqlx::query_scalar("SELECT deleted_at IS NOT NULL FROM quotes WHERE id = ?")
            .bind(copy_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(deleted);
}

#[test_db::test(seed)]
async fn without_edit_quotes_the_phone_is_told_and_refused(pool: MySqlPool) {
    // Rita is a sales rep whose company took Edit quotes from sales reps.
    add_session(&pool, 314, 100, 100).await;
    sql(
        &pool,
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (100, 1, 100)",
    )
    .await;
    let (server, _) = phone_server(pool.clone()).await;
    let bootstrap = server
        .get("/v1/phone/bootstrap")
        .add_header(HEADER, token(314))
        .await
        .json::<Value>();
    assert_eq!(
        bootstrap["user"]["skills"],
        json!(["sell_slabs", "edit_transactions", "edit_quotes"])
    );
    sql(
        &pool,
        "INSERT INTO company_position_sections (company_id, position_id, sections, skills)
           VALUES (100, 1, 'customers,deals', 'sell_slabs')",
    )
    .await;
    let bootstrap = server
        .get("/v1/phone/bootstrap")
        .add_header(HEADER, token(314))
        .await
        .json::<Value>();
    assert_eq!(bootstrap["user"]["skills"], json!(["sell_slabs"]));
    // Her phone may still read quotes but not change them.
    server
        .get("/v1/quotes")
        .add_header(HEADER, token(314))
        .await
        .assert_status_ok();
    server
        .post("/v1/quotes")
        .add_header(HEADER, token(314))
        .json(&json!({ "customer_id": 1000, "quote_name": "Kitchen" }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn the_quote_screen_prints_company_forms_and_keeps_its_email_templates(pool: MySqlPool) {
    // Rita (user 100, company 100) on her phone; the company has a form.
    add_session(&pool, 313, 100, 100).await;
    let (server, _stubs) = phone_server(pool.clone()).await;
    let form = server
        .post("/v1/quote-forms")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Contract", "config": { "kind": "contract", "header": {}, "rows": [] } }))
        .await;
    form.assert_status(StatusCode::CREATED);
    let form_id = form.json::<Value>()["id"].as_i64().unwrap();

    // The form's layout and the letterhead read as with her cookie, to print it.
    for read in [
        format!("/v1/quote-forms/{form_id}"),
        "/v1/quote-forms/letterhead".to_string(),
    ] {
        let by_phone = server.get(&read).add_header(HEADER, token(313)).await;
        by_phone.assert_status_ok();
        let by_cookie = server.get(&read).with_session(sessions::REP).await;
        assert_eq!(
            by_phone.json::<Value>(),
            by_cookie.json::<Value>(),
            "{read}"
        );
    }

    // Her own email template: made, changed and deleted from the Email sheet.
    let made = server
        .post("/v1/me/quote-email-templates")
        .add_header(HEADER, token(313))
        .json(&json!({ "name": "Follow up", "subject": "Your quote", "body": "Hi" }))
        .await;
    made.assert_status(StatusCode::CREATED);
    let template = made.json::<Value>()["id"].as_i64().unwrap();
    server
        .put(&format!("/v1/me/quote-email-templates/{template}"))
        .add_header(HEADER, token(313))
        .json(&json!({ "name": "Follow up", "subject": "Your new quote", "body": "Hi again" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let mine = server
        .get("/v1/me/quote-email-templates")
        .add_header(HEADER, token(313))
        .await;
    mine.assert_status_ok();
    let listed = mine.json::<Value>()["templates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"].as_i64() == Some(template))
        .cloned()
        .expect("her template is listed");
    assert_eq!(listed["subject"], "Your new quote");
    server
        .delete(&format!("/v1/me/quote-email-templates/{template}"))
        .add_header(HEADER, token(313))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    // Changing the company's forms still needs the CRM session.
    for (method, route) in [
        ("PUT", format!("/v1/quote-forms/{form_id}")),
        ("POST", "/v1/quote-forms".to_string()),
        ("DELETE", format!("/v1/quote-forms/{form_id}")),
    ] {
        let request = match method {
            "POST" => server.post(&route).json(&json!({})),
            "PUT" => server.put(&route).json(&json!({})),
            _ => server.delete(&route),
        };
        request
            .add_header(HEADER, token(313))
            .await
            .assert_status(StatusCode::FORBIDDEN);
    }
}

#[test_db::test(seed)]
async fn the_sale_form_sells_slabs_with_its_device_token(pool: MySqlPool) {
    // Rita (user 100, company 100) on her phone, after a quote is sold: the
    // sale form reads the free sinks, faucets and slabs of stone 3010 and the
    // stone types, and sells slab 6070 with sink 3100 and faucet 3200.
    add_session(&pool, 320, 100, 100).await;
    let (server, _stubs) = phone_server(pool.clone()).await;
    for read in [
        "/v1/sinks?available_only=true",
        "/v1/faucets?available_only=true",
        "/v1/slabs?id=6070&id=6074",
        "/v1/stone-types",
        "/v1/stones/3010/slabs",
        "/v1/stones/search?name=Taj&include_sold_out=true",
        "/v1/customers/1000/sales",
        // Fewer than 3 letters: no lookup, nothing suggested.
        "/v1/places/autocomplete?q=ab",
    ] {
        let by_phone = server.get(read).add_header(HEADER, token(320)).await;
        by_phone.assert_status_ok();
        let by_cookie = server.get(read).with_session(sessions::REP).await;
        assert_eq!(
            by_phone.json::<Value>(),
            by_cookie.json::<Value>(),
            "{read}"
        );
    }
    let sale = json!({
        "customer_id": 1000,
        "seller_id": 100,
        "project_address": "1 Main St, Carmel, IN 46032",
        "notes": "From the quote",
        "price": 2449.99,
        "extras": [],
        "rooms": [{
            "room_id": "0192f1a0-0000-7000-8000-0000000000c1",
            "room": "kitchen", "backsplash": "4 inch", "square_feet": 42.5,
            "retail_price": 50, "tear_out": "no", "stove": "f/s", "waterfall": "no",
            "corbels": 0, "seam": "standard", "ten_year_sealer": false,
            "extras": { "edge_price": { "edge_type": "flat", "price": 0 } },
            "slabs": [{ "id": 6070, "is_full": true }],
            "sinks": [{ "type_id": 3100, "price": 199 }],
            "faucets": [{ "type_id": 3200, "price": 125.99 }],
        }],
    });
    let made = server
        .post("/v1/sales")
        .add_header(HEADER, token(320))
        .json(&sale)
        .await;
    made.assert_status(StatusCode::CREATED);
    let sale_id = made.json::<Value>()["id"].as_i64().unwrap();
    let sold: (Option<i64>, Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT CAST(i.sale_id AS SIGNED), CAST(s.customer_id AS SIGNED), CAST(s.seller_id AS SIGNED)
           FROM slab_inventory i JOIN sales s ON s.id = i.sale_id WHERE i.id = 6070",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(sold, (Some(sale_id), Some(1000), Some(100)));
    let sink: Option<i64> =
        sqlx::query_scalar("SELECT CAST(slab_id AS SIGNED) FROM sinks WHERE id = 6100")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(sink, Some(6070));

    // Add New Color: a stone with one slab per bundle, ready to sell.
    let quick = server
        .post("/v1/stones/quick")
        .add_header(HEADER, token(320))
        .json(
            &json!({ "name": "Phone Granite", "type": "granite", "retail_price": 55,
                       "length": 120, "width": 60, "leftover": false, "bundles": ["B-1"] }),
        )
        .await;
    quick.assert_status(StatusCode::CREATED);
    assert_eq!(
        quick.json::<Value>()["slab_ids"].as_array().unwrap().len(),
        1
    );

    // Editing or cancelling the sale stays with the CRM session.
    server
        .put(&format!("/v1/sales/{sale_id}"))
        .add_header(HEADER, token(320))
        .json(&sale)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .post(&format!("/v1/sales/{sale_id}/unsell"))
        .add_header(HEADER, token(320))
        .json(&json!({}))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .post("/v1/sinks")
        .add_header(HEADER, token(320))
        .json(&json!({ "name": "Nope" }))
        .await
        .assert_status(StatusCode::FORBIDDEN);

    // Without the Sell slabs skill her phone sells nothing, as on the web.
    sql(
        &pool,
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (100, 1, 100)",
    )
    .await;
    sql(
        &pool,
        "INSERT INTO company_position_sections (company_id, position_id, sections, skills)
           VALUES (100, 1, 'customers,deals,stones', 'edit_quotes')",
    )
    .await;
    let mut other = sale.clone();
    other["rooms"][0]["slabs"] = json!([{ "id": 6074, "is_full": true }]);
    other["rooms"][0]["sinks"] = json!([]);
    other["rooms"][0]["faucets"] = json!([]);
    let refused = server
        .post("/v1/sales")
        .add_header(HEADER, token(320))
        .json(&other)
        .await;
    refused.assert_status(StatusCode::FORBIDDEN);
    assert!(
        refused.json::<Value>()["error"]
            .as_str()
            .unwrap_or_default()
            .contains("Sell slabs"),
    );
}

/// A QuickBooks stand-in for company 100 (realm 555): customer 1000
/// (alice@example.com) is QuickBooks customer 42; it has products 3 Stone and
/// 5 tile and class Rita; each invoice posted is kept and made invoice 9, 10…
async fn quickbooks_stub(pool: &MySqlPool) -> (api::qbo::Qbo, Arc<Mutex<Vec<Value>>>) {
    use ::common::telephony::ringcentral_auth::encrypt;
    use axum::extract::Query;
    use axum::routing::{get, post};
    const KEY: [u8; 32] = [7; 32];
    let now = chrono::Utc::now().timestamp();
    sqlx::query(
        "UPDATE company SET qbo_realm_id = '555', qbo_access_token = ?, qbo_refresh_token = ?,
                qbo_access_expires = ?, qbo_refresh_expires = ?, qbo_client_id = ?, qbo_client_secret = ?
          WHERE id = 100",
    )
    .bind(encrypt(&KEY, "access"))
    .bind(encrypt(&KEY, "refresh"))
    .bind(now + 3600)
    .bind(now + 86_400)
    .bind(encrypt(&KEY, "client-id"))
    .bind(encrypt(&KEY, "client-secret"))
    .execute(pool)
    .await
    .unwrap();
    let posted = Arc::new(Mutex::new(Vec::<Value>::new()));
    let kept = Arc::clone(&posted);
    let router = axum::Router::new()
        .route(
            "/v3/company/555/query",
            get(|Query(query): Query<HashMap<String, String>>| async move {
                let q = &query["query"];
                let rows = if q.contains("PrimaryEmailAddr = 'alice@example.com'") {
                    json!({ "Customer": [{ "Id": "42", "DisplayName": "Alice Lead" }] })
                } else if q.starts_with("select * from Item where Active = true") {
                    json!({ "Item": [
                        { "Id": "5", "Name": "tile", "Type": "Service" },
                        { "Id": "3", "Name": "Stone", "Type": "NonInventory" },
                    ] })
                } else if q.starts_with("select Id, CustomerRef from Invoice where Id = '9'") {
                    json!({ "Invoice": [{ "Id": "9", "CustomerRef": { "value": "42" } }] })
                } else if q.contains("from Class") {
                    json!({ "Class": [{ "Id": "1", "Name": "Rita", "FullyQualifiedName": "Rita" }] })
                } else {
                    json!({})
                };
                axum::Json(json!({ "QueryResponse": rows }))
            }),
        )
        .route(
            "/v3/company/555/invoice",
            post(move |axum::Json(body): axum::Json<Value>| {
                let kept = Arc::clone(&kept);
                async move {
                    let mut saved = kept.lock().unwrap();
                    saved.push(body.clone());
                    let id = 8 + saved.len();
                    let lines: Vec<Value> = body["Line"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .enumerate()
                        .map(|(i, mut line)| {
                            line["Id"] = json!((i + 1).to_string());
                            line
                        })
                        .collect();
                    axum::Json(json!({ "Invoice": {
                        "Id": id.to_string(), "SyncToken": "0", "DocNumber": format!("10{id:02}"),
                        "TxnDate": "2026-11-06", "TotalAmt": 500, "CustomerRef": { "value": "42" },
                        "Line": lines,
                    } }))
                }
            }),
        )
        .route(
            "/v3/company/555/invoice/{id}/pdf",
            get(|| async { ([("content-type", "application/pdf")], b"%PDF-qbo".to_vec()) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let qbo = api::qbo::Qbo::new(Some(KEY), None, &base, &format!("{base}/token"), false);
    (qbo, posted)
}

#[test_db::test(seed)]
async fn the_invoice_editor_makes_the_sold_quotes_quickbooks_invoice_with_its_device_token(
    pool: MySqlPool,
) {
    // Rita's quote for Alice was sold; her phone opens its QuickBooks invoice.
    add_session(&pool, 321, 100, 100).await;
    sql(
        &pool,
        "INSERT INTO quotes (id, customer_id, quote_name, quote_type, created_date, sales_rep,
                             company_id, salesperson_id, created_by, status)
         VALUES (9811, 1000, 'Kitchen', 'countertops', NOW(), 'Rep', 100, 100, 100, 'converted')",
    )
    .await;
    let (qbo, posted) = quickbooks_stub(&pool).await;
    let (server, _stubs) = phone_server_with(pool.clone(), |state| state.with_qbo(qbo)).await;

    // The products, classes and usual split read as with her cookie.
    for read in ["/v1/qbo/items", "/v1/qbo/invoice-split?parts=stone"] {
        let by_phone = server.get(read).add_header(HEADER, token(321)).await;
        by_phone.assert_status_ok();
        let by_cookie = server.get(read).with_session(sessions::REP).await;
        assert_eq!(
            by_phone.json::<Value>(),
            by_cookie.json::<Value>(),
            "{read}"
        );
    }
    let line = |id: &str, detail: &str, description: &str, amount: Value| {
        json!({ "id": id, "line_num": null, "detail_type": detail, "description": description,
                "amount": amount, "qty": if amount.is_null() { Value::Null } else { json!(1) },
                "unit_price": amount,
                "item_ref_value": if amount.is_null() { Value::Null } else { json!("3") },
                "item_ref_name": null, "editable": true })
    };
    let lines = json!([
        line(
            "new:quote-0",
            "SalesItemLineDetail",
            "Kitchen countertops",
            json!(500)
        ),
        line("new:quote-1", "DescriptionOnly", "Flat edge", Value::Null),
    ]);
    // The company's usual changes made to the draft.
    let drafted = server
        .post("/v1/quotes/9811/invoice-draft")
        .add_header(HEADER, token(321))
        .json(&json!({ "lines": lines }))
        .await;
    drafted.assert_status_ok();
    assert_eq!(
        drafted.json::<Value>()["lines"].as_array().unwrap().len(),
        2
    );

    let invoice = json!({
        "id": "", "sync_token": "", "doc_number": null, "txn_date": "2026-11-06",
        "due_date": null, "total_amt": 500, "balance": null, "currency": null,
        "customer_memo": null, "private_note": "Quote: Kitchen", "bill_addr": null,
        "ship_addr": null, "lines": lines, "class_name": "Rita",
    });
    let parts = json!([{ "line_id": "new:quote-0", "key": "stone" }]);
    let created = server
        .post("/v1/customers/1000/qbo-invoices")
        .add_header(HEADER, token(321))
        .json(&json!({ "invoice": invoice, "quote_id": 9811, "draft": lines, "parts": parts }))
        .await;
    created.assert_status_ok();
    assert_eq!(created.json::<Value>()["id"], "9");
    assert_eq!(posted.lock().unwrap()[0]["CustomerRef"]["value"], "42");
    let linked: Option<String> =
        sqlx::query_scalar("SELECT qbo_invoice_id FROM quotes WHERE id = 9811")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(linked.as_deref(), Some("9"));

    // Split invoice: a deposit and the balance.
    let share = |amount: f64| json!([{ "line_id": "new:quote-0", "amount": amount }]);
    let split = server
        .post("/v1/customers/1000/qbo-invoices/split")
        .add_header(HEADER, token(321))
        .json(&json!({
            "invoice": invoice, "quote_id": 9811, "draft": lines, "parts": parts,
            "invoices": [
                { "label": "Deposit", "shares": share(250.0) },
                { "label": null, "shares": share(250.0) },
            ],
        }))
        .await;
    split.assert_status_ok();
    assert_eq!(
        split.json::<Value>()["invoices"].as_array().unwrap().len(),
        2
    );
    assert_eq!(posted.lock().unwrap().len(), 3);

    // "Keep in sync with the quote", read back as with her cookie.
    let link = json!({ "option_index": 0, "kind": "qbo", "invoice_ids": ["10", "11"],
                       "detail": { "layout": [], "lines": {}, "sent": [] },
                       "source_hash": "0123456789abcdef", "enabled": true });
    server
        .put("/v1/quotes/9811/invoice-sync")
        .add_header(HEADER, token(321))
        .json(&link)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let kept: Value = server
        .get("/v1/quotes/9811/invoice-sync")
        .add_header(HEADER, token(321))
        .await
        .json();
    assert_eq!(kept["invoice_ids"], json!(["10", "11"]));

    // Its PDF, for Create and send.
    let pdf = server
        .get("/v1/customers/1000/qbo-invoices/9/pdf")
        .add_header(HEADER, token(321))
        .await;
    pdf.assert_status_ok();
    assert_eq!(pdf.as_bytes().as_ref(), b"%PDF-qbo");

    // Editing an invoice, AI edits and the invoice settings stay with the CRM.
    for (method, route) in [
        ("PUT", "/v1/customers/1000/qbo-invoices/9"),
        ("POST", "/v1/ai/invoice-edit"),
        ("PUT", "/v1/qbo/invoice-instructions"),
        ("GET", "/v1/qbo/invoice-instructions"),
    ] {
        let request = match method {
            "PUT" => server.put(route).json(&json!({})),
            "POST" => server.post(route).json(&json!({})),
            _ => server.get(route),
        };
        request
            .add_header(HEADER, token(321))
            .await
            .assert_status(StatusCode::FORBIDDEN);
    }
    assert_eq!(posted.lock().unwrap().len(), 3);
}

#[test_db::test(seed)]
async fn the_sale_form_reads_the_deal_owner_with_its_device_token(pool: MySqlPool) {
    // Deal 8008 is Mark's (user 101): a quote without a salesperson sells as
    // his, so Rita's phone reads the deal's owner as the website does.
    add_session(&pool, 322, 100, 100).await;
    let (server, _stubs) = phone_server(pool.clone()).await;
    let by_phone = server
        .get("/v1/deals/8008")
        .add_header(HEADER, token(322))
        .await;
    by_phone.assert_status_ok();
    let by_phone: Value = by_phone.json();
    assert_eq!(by_phone["user_id"], 101);
    let by_cookie: Value = server
        .get("/v1/deals/8008")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(by_phone, by_cookie);
}

#[test_db::test(seed)]
async fn the_own_invoice_editor_makes_a_quotes_invoice_with_its_device_token(pool: MySqlPool) {
    // Company 100 is not on QuickBooks: Rita's phone makes the quote's invoice
    // the CRM keeps itself, emails it and marks it sent.
    add_session(&pool, 323, 100, 100).await;
    sql(
        &pool,
        "INSERT INTO quotes (id, customer_id, quote_name, quote_type, created_date, sales_rep,
                             company_id, salesperson_id, created_by, status)
         VALUES (9812, 1000, 'Kitchen', 'countertops', NOW(), 'Rep', 100, 100, 100, 'converted')",
    )
    .await;
    let (server, _stubs) = phone_server(pool.clone()).await;

    // The number the new invoice gets, as with her cookie.
    let by_phone = server
        .get("/v1/invoices")
        .add_header(HEADER, token(323))
        .await;
    by_phone.assert_status_ok();
    let by_phone: Value = by_phone.json();
    let by_cookie: Value = server
        .get("/v1/invoices")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(by_phone, by_cookie);
    let next = by_phone["next_number"].as_i64().unwrap();

    let created = server
        .post("/v1/invoices")
        .add_header(HEADER, token(323))
        .json(&json!({
            "invoice_number": null, "customer_id": 1000, "quote_id": 9812,
            "invoice_date": "2026-11-06", "due_date": "2026-11-20",
            "bill_to_name": "Alice Lead", "bill_to_email": "alice@example.com",
            "project_address": "1 Main St, Carmel, IN 46032",
            "lines": [
                { "description": "Kitchen countertops", "qty": 1, "unit_price": 2814 },
                { "description": "42 sq ft Quartz", "qty": null, "unit_price": null },
            ],
            "tax": 197.0, "customer_memo": "Payment Terms\nDue on install",
            "private_note": null,
        }))
        .await;
    created.assert_status(StatusCode::CREATED);
    let invoice: Value = created.json();
    assert_eq!(invoice["invoice_number"], next);
    assert_eq!(invoice["status"], "draft");
    assert_eq!(invoice["quote_id"], 9812);
    assert_eq!(invoice["total"], 3011.0);
    let id = invoice["id"].as_i64().unwrap();

    // "Keep in sync with the quote" links it, as an own invoice.
    server
        .put("/v1/quotes/9812/invoice-sync")
        .add_header(HEADER, token(323))
        .json(
            &json!({ "option_index": 0, "kind": "own", "invoice_ids": [id.to_string()],
                       "detail": {}, "source_hash": "0123456789abcdef", "enabled": true }),
        )
        .await
        .assert_status(StatusCode::NO_CONTENT);

    // Once the email with its PDF goes, it is marked sent.
    let sent = server
        .put(&format!("/v1/invoices/{id}/status"))
        .add_header(HEADER, token(323))
        .json(&json!({ "status": "sent" }))
        .await;
    sent.assert_status_ok();
    assert_eq!(sent.json::<Value>()["status"], "sent");
    let status: String = sqlx::query_scalar("SELECT status FROM invoices WHERE id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "sent");

    // Opening or editing an invoice stays with the CRM session.
    server
        .get(&format!("/v1/invoices/{id}"))
        .add_header(HEADER, token(323))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .put(&format!("/v1/invoices/{id}"))
        .add_header(HEADER, token(323))
        .json(&json!({}))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .get(&format!("/v1/invoices/{id}"))
        .with_session(sessions::REP)
        .await
        .assert_status_ok();
}

#[test_db::test(seed)]
async fn the_deal_edit_saves_its_customer_and_reassigns_the_sales_rep_with_its_device_token(
    pool: MySqlPool,
) {
    // Rita (100) and Mark (101, the sales manager) have the app. Alice Lead
    // (1000) is Rita's, with her deals 8000, 8004 and 8007.
    add_phone(&pool, 330, 100, 100).await;
    add_phone(&pool, 331, 101, 100).await;
    sql(
        &pool,
        "INSERT INTO customer_sources (company_id, source_key, label, is_system)
         VALUES (100, 'leads', 'Leads', 1), (100, 'Home show', 'Home show', 0)",
    )
    .await;
    let (server, stubs) = phone_server(pool.clone()).await;

    // The Source list of the website's customer form.
    let sources = server
        .get("/v1/customer-sources")
        .add_header(HEADER, token(330))
        .await;
    sources.assert_status_ok();
    let sources: Vec<Value> = sources.json();
    let keys: Vec<&Value> = sources.iter().map(|row| &row["source_key"]).collect();
    assert_eq!(keys, [&json!("leads"), &json!("Home show")], "{sources:?}");

    // Rita's phone saves the deal's customer as the website's Edit Customer
    // does, giving Alice to Mark.
    server
        .put("/v1/customers/1000")
        .add_header(HEADER, token(330))
        .json(&json!({
            "name": "Alice Lead",
            "phone": "317-555-0100",
            "phone_2": null,
            "emails": ["alice@example.com", "alice.new@example.com"],
            "address": "9 Oak St",
            "your_message": "Wants it before Christmas",
            "company_name": null,
            "source": "Home show",
            "sales_rep": 101
        }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let customer: Value = server
        .get("/v1/customers/1000")
        .add_header(HEADER, token(331))
        .await
        .json();
    assert_eq!(customer["sales_rep"], 101);
    assert_eq!(customer["sales_rep_name"], "Mark Manager");
    assert_eq!(customer["address"], "9 Oak St");
    assert_eq!(customer["source"], "Home show");
    assert_eq!(customer["phone_2"], Value::Null);
    assert_eq!(customer["your_message"], "Wants it before Christmas");
    assert_eq!(
        customer["emails"],
        json!(["alice@example.com", "alice.new@example.com"])
    );
    // Mark has her deals now, and his phone is told (Rita gave it: hers is not).
    let left_with_rita: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM deals WHERE customer_id = 1000 AND deleted_at IS NULL AND user_id = 100",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(left_with_rita, 0);
    let leads: Vec<Value> = stubs
        .pushes()
        .into_iter()
        .filter(|push| push["data"]["type"] == "lead")
        .collect();
    assert_eq!(leads.len(), 1, "{leads:?}");
    assert_eq!(leads[0]["token"], "push-331");
    assert_eq!(leads[0]["data"]["kind"], "assigned");
    assert_eq!(leads[0]["data"]["customerId"], "1000");
    // The audit trail says who moved it.
    let moved: Option<String> = sqlx::query_scalar(
        "SELECT reassigned_to FROM customers_history WHERE customer_id = 1000
          ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(moved.as_deref(), Some("Mark Manager"));

    // Everything else about a customer stays with the CRM session: 403, so
    // the phone is not signed out.
    for (method, path) in [
        ("DELETE", "/v1/customers/1000"),
        ("POST", "/v1/customers"),
        ("PUT", "/v1/customers/1000/temperature"),
        ("POST", "/v1/customer-sources"),
    ] {
        let request = match method {
            "DELETE" => server.delete(path),
            "POST" => server.post(path),
            _ => server.put(path),
        };
        request
            .add_header(HEADER, token(330))
            .json(&json!({ "name": "X", "source": "leads", "temperature": "hot" }))
            .await
            .assert_status(StatusCode::FORBIDDEN);
    }
}
