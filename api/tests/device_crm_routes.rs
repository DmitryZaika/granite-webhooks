//! The Granite Phone app's Emails and Schedule screens call the CRM's own
//! routes (`/v1/emails`, `/v1/schedule`, `/v1/events`) with its device token
//! (`X-Granite-Device`) instead of the cookie (`auth::DEVICE_ROUTES`), and so
//! does its schedule-only app for installers and template makers
//! (`/v1/me/schedule`).
//!
//! Fixture phones use n = 301..=310; events 9750+.

mod common;
mod phone_common;

use axum::http::StatusCode;
use common::{WithSession, sessions};
use phone_common::{HEADER, add_session, phone_server, revoke_session, sql, token};
use serde_json::{Value, json};
use sqlx::MySqlPool;

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
    // The device token stops at /v1/me/schedule: the rest of /v1/me needs the cookie.
    for path in ["/v1/me", "/v1/me/todos"] {
        server
            .get(path)
            .add_header(HEADER, token(304))
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

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
async fn the_app_reads_emails_and_the_schedule_with_its_device_token(pool: MySqlPool) {
    // Rita (user 100, company 100) signed in on her phone.
    add_session(&pool, 301, 100, 100).await;
    let (server, _) = phone_server(pool.clone()).await;

    // The same answers as her CRM session gets.
    for path in [UNREAD, EVENTS] {
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

    // Nothing without a token or cookie; a bad, revoked or idle token is out
    // even with a cookie (the header decides alone).
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
        .assert_status(StatusCode::UNAUTHORIZED);

    // Elsewhere the cookie routes still need the cookie, even right next to
    // the prefixes (`/v1/email-attachments` is not under `/v1/emails`).
    for path in ["/v1/company/features", "/v1/email-attachments/1/preview"] {
        server
            .get(path)
            .add_header(HEADER, token(301))
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }
    // The app's own routes took the token everywhere before, and still do.
    server
        .get("/v1/me/devices")
        .add_header(HEADER, token(301))
        .await
        .assert_status_ok();
}

#[test_db::test(seed)]
async fn the_composer_reads_its_lists_but_cannot_change_them(pool: MySqlPool) {
    // Rita (user 100, company 100) signed in on her phone.
    add_session(&pool, 305, 100, 100).await;
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

    // Those lists are read-only to the phone: a write needs the CRM session.
    server
        .post("/v1/email-templates")
        .add_header(HEADER, token(305))
        .json(&json!({ "template_name": "x", "template_subject": "x", "template_body": "x" }))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    server
        .delete("/v1/calendars/1")
        .add_header(HEADER, token(305))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    server
        .delete("/v1/customers/1000")
        .add_header(HEADER, token(305))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);

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
