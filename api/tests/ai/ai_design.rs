//! `GET /v1/ai/design/pictures` and `GET /v1/ai/design/deals`: what the
//! chat's AI design tool picks from.

use crate::common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

/// Rita (rep@acme.test) received pictures from Alice (customer 1000) and an
/// unknown sender; others got mail she must not see.
async fn mail(pool: &MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO emails (id, subject, body, sent_at, message_id, thread_id, sender_user_id, receiver_user_id,
             sender_email, receiver_email, company_id, is_draft) VALUES
           (9700, 'Kitchen', 'Photo', '2026-05-01 10:00:00', '<m9700@x>', 'bbbbbbbb-0000-4000-8000-000000009700', NULL, 100,
            'Alice Lead <alice@example.com>', 'rep@acme.test', 100, 0),
           (9701, 'More', 'Photos', '2026-05-02 10:00:00', '<m9701@x>', 'bbbbbbbb-0000-4000-8000-000000009701', NULL, NULL,
            'stranger@x.test', 'Rita <REP@acme.test>', 100, 0),
           (9702, 'Not hers', 'Photo', '2026-05-03 10:00:00', '<m9702@x>', 'bbbbbbbb-0000-4000-8000-000000009702', NULL, 101,
            'alice@example.com', 'manager@acme.test', 100, 0),
           (9703, 'Sent by her', 'Photo', '2026-05-04 10:00:00', '<m9703@x>', 'bbbbbbbb-0000-4000-8000-000000009703', 100, NULL,
            'rep@acme.test', 'rep@acme.test', 100, 0),
           (9704, 'Trashed', 'Photo', '2026-05-05 10:00:00', '<m9704@x>', 'bbbbbbbb-0000-4000-8000-000000009704', NULL, 100,
            'alice@example.com', 'rep@acme.test', 100, 0);
         UPDATE emails SET deleted_at = '2026-05-06 10:00:00' WHERE id = 9704;
         INSERT INTO email_attachments (id, email_id, content_type, content_subtype, filename, url) VALUES
           (9710, 9700, 'image', 'jpeg', 'kitchen.jpg', 'https://files.test/kitchen.jpg'),
           (9711, 9700, 'application', 'pdf', 'quote.pdf', 'https://files.test/quote.pdf'),
           (9712, 9701, 'application', 'heic', 'IMG_1.HEIC', 'https://files.test/img1.heic'),
           (9713, 9701, 'image', 'png', '', 'https://files.test/blank.png'),
           (9714, 9702, 'image', 'png', 'manager.png', 'https://files.test/m.png'),
           (9715, 9703, 'image', 'png', 'mine.png', 'https://files.test/mine.png'),
           (9716, 9704, 'image', 'png', 'trash.png', 'https://files.test/t.png');",
    )
    .execute(pool)
    .await
    .unwrap();
}

fn ids(body: &Value) -> Vec<i64> {
    body["pictures"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_i64().unwrap())
        .collect()
}

#[test_db::test(seed)]
async fn the_callers_received_pictures_newest_first(pool: MySqlPool) {
    mail(&pool).await;
    let server = server(pool);
    let body: Value = server
        .get("/v1/ai/design/pictures")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(ids(&body), [9713, 9712, 9710]);
    assert_eq!(body["has_more"], false);
    let pictures = body["pictures"].as_array().unwrap();
    assert_eq!(pictures[0]["filename"], "customer-image-9713");
    assert_eq!(pictures[2]["customer_name"], "Alice Lead");
    assert_eq!(
        pictures[2]["sender_email"],
        "Alice Lead <alice@example.com>"
    );
    assert_eq!(pictures[1]["customer_name"], Value::Null);
    // The manager sees their own mail only.
    let theirs: Value = server
        .get("/v1/ai/design/pictures")
        .with_session(sessions::MANAGER)
        .await
        .json();
    assert_eq!(ids(&theirs), [9714]);
}

#[test_db::test(seed)]
async fn pictures_page_and_search(pool: MySqlPool) {
    mail(&pool).await;
    let server = server(pool);
    let page: Value = server
        .get("/v1/ai/design/pictures?limit=2")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(ids(&page), [9713, 9712]);
    assert_eq!(page["has_more"], true);
    let next: Value = server
        .get("/v1/ai/design/pictures?limit=2&offset=2")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(ids(&next), [9710]);
    assert_eq!(next["has_more"], false);
    for (search, expected) in [
        ("Alice", vec![9710]),
        ("stranger", vec![9713, 9712]),
        ("IMG_", vec![9712]),
    ] {
        let found: Value = server
            .get(&format!("/v1/ai/design/pictures?search={search}"))
            .with_session(sessions::REP)
            .await
            .json();
        assert_eq!(ids(&found), expected, "{search}");
    }
}

#[test_db::test(seed)]
async fn the_callers_own_deals_newest_first(pool: MySqlPool) {
    let server = server(pool);
    let deals: Value = server
        .get("/v1/ai/design/deals")
        .with_session(sessions::REP)
        .await
        .json();
    let rows = deals.as_array().unwrap();
    assert!(!rows.is_empty());
    let ids: Vec<i64> = rows.iter().map(|d| d["id"].as_i64().unwrap()).collect();
    assert!(ids.windows(2).all(|w| w[0] > w[1]), "{ids:?}");
    assert!(!ids.contains(&8005), "deleted deal listed");
    assert!(!ids.contains(&8600), "another rep's deal listed");
    assert!(
        rows.iter().any(|d| d
            == &json!({ "id": 8000, "title": "Alice kitchen", "customer_name": "Alice Lead" }))
    );
    server
        .get("/v1/ai/design/deals")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    server
        .get("/v1/ai/design/pictures")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}
