mod common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::json;
use sqlx::MySqlPool;

async fn chain(pool: &MySqlPool, parent: Option<i32>) -> Vec<(i32, Option<i32>)> {
    sqlx::query_as("SELECT id, after_id FROM instructions WHERE parent_id <=> ? AND company_id = 100 ORDER BY id")
        .bind(parent)
        .fetch_all(pool)
        .await
        .unwrap()
}

#[test_db::test(seed)]
async fn admins_reorder_siblings(pool: MySqlPool) {
    sqlx::query(
        "INSERT INTO instructions (id, title, parent_id, after_id, company_id) VALUES
           (500, 'A', NULL, NULL, 100), (501, 'B', NULL, 500, 100), (502, 'C', NULL, 501, 100),
           (503, 'AI follow-up instruction', NULL, NULL, 100),
           (510, 'A1', 500, NULL, 100), (520, 'Other', NULL, NULL, 101)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool.clone());
    let put = |session: &'static str, body: serde_json::Value| {
        let request = server
            .put("/v1/instructions/order")
            .with_session(session)
            .json(&body);
        async move { request.await }
    };
    put(
        sessions::REP,
        json!({ "parent_id": null, "ordered_ids": [502, 500, 501] }),
    )
    .await
    .assert_status(StatusCode::FORBIDDEN);
    put(
        sessions::ADMIN,
        json!({ "parent_id": null, "ordered_ids": [502, 500, 501] }),
    )
    .await
    .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        chain(&pool, None).await,
        [(500, Some(502)), (501, Some(500)), (502, None), (503, None)]
    );
    for bad in [
        json!({ "parent_id": null, "ordered_ids": [] }),
        json!({ "parent_id": null, "ordered_ids": [500, 501] }),
        json!({ "parent_id": null, "ordered_ids": [500, 500, 501, 502] }),
        json!({ "parent_id": null, "ordered_ids": [500, 501, 502, 520] }),
        json!({ "parent_id": 500, "ordered_ids": [500] }),
    ] {
        put(sessions::ADMIN, bad)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    put(
        sessions::ADMIN,
        json!({ "parent_id": 500, "ordered_ids": [510] }),
    )
    .await
    .assert_status(StatusCode::NO_CONTENT);
}

async fn seed_tree(pool: &MySqlPool) {
    // General instructions use company 0, which the seed does not have.
    sqlx::raw_sql(
        "SET FOREIGN_KEY_CHECKS = 0;
         INSERT INTO instructions (id, title, parent_id, after_id, rich_text, company_id) VALUES
           (600, 'Welcome', NULL, NULL, '<p>Hi</p>', 100),
           (601, 'Sales only', NULL, 600, '<p>Pricing</p>', 100),
           (602, 'Shop only', NULL, 601, '<p>Saws</p>', 100),
           (603, 'Theirs', NULL, NULL, '', 101),
           (604, 'Everyone', NULL, NULL, '<p>General</p>', 0);
         SET FOREIGN_KEY_CHECKS = 1;
         INSERT INTO instruction_access (instruction_id, position_id) VALUES (601, 1), (602, 4);",
    )
    .execute(pool)
    .await
    .unwrap();
}

fn titles(rows: &serde_json::Value) -> Vec<&str> {
    rows.as_array()
        .unwrap()
        .iter()
        .map(|row| row["title"].as_str().unwrap())
        .collect()
}

#[test_db::test(seed)]
async fn readers_see_what_their_positions_open(pool: MySqlPool) {
    seed_tree(&pool).await;
    let server = server(pool);
    // Rita is a sales rep (position 1).
    let rows: serde_json::Value = server
        .get("/v1/instructions")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(titles(&rows), ["Welcome", "Sales only"]);
    assert_eq!(rows[1]["after_id"], 600);
    assert_eq!(rows[1]["position_ids"], json!([1]));
    let general: serde_json::Value = server
        .get("/v1/instructions?scope=general")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(titles(&general), ["Everyone"]);
    // Admins manage every row; only super admins the general tree.
    let all: serde_json::Value = server
        .get("/v1/instructions?manage=true")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(titles(&all), ["Welcome", "Sales only", "Shop only"]);
    server
        .get("/v1/instructions?manage=true")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .get("/v1/instructions?manage=true&scope=general")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn admins_add_edit_and_delete_instructions(pool: MySqlPool) {
    seed_tree(&pool).await;
    let server = server(pool.clone());
    let res = server
        .post("/v1/instructions")
        .with_session(sessions::ADMIN)
        .json(&json!({
            "title": " Pricing basics ",
            "parent_id": null,
            "after_id": 600,
            "rich_text": "<p>x</p>",
            "position_ids": [1, 9, 1, 8],
        }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let id = res.json::<serde_json::Value>()["id"].as_i64().unwrap();
    let id32 = i32::try_from(id).unwrap();
    // It sits after 600; the sibling that followed 600 now follows it.
    assert_eq!(
        chain(&pool, None).await,
        [
            (600, None),
            (601, Some(id32)),
            (602, Some(601)),
            (id32, Some(600))
        ]
    );
    let access: Vec<i32> =
        sqlx::query_scalar("SELECT position_id FROM instruction_access WHERE instruction_id = ?")
            .bind(id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(access, [1]);

    let res = server
        .put(&format!("/v1/instructions/{id}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "title": "Pricing", "parent_id": 600, "after_id": null, "rich_text": "<p>y</p>", "position_ids": [] }))
        .await;
    res.assert_status(StatusCode::NO_CONTENT);
    let (title, parent): (String, Option<i32>) =
        sqlx::query_as("SELECT title, parent_id FROM instructions WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((title.as_str(), parent), ("Pricing", Some(600)));

    // Other companies' and the general tree are not the admin's.
    for path in ["/v1/instructions/603", "/v1/instructions/604"] {
        server
            .delete(path)
            .with_session(sessions::ADMIN)
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }
    server
        .post("/v1/instructions")
        .with_session(sessions::ADMIN)
        .json(&json!({ "title": "Bad", "parent_id": 603 }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .post("/v1/instructions")
        .with_session(sessions::REP)
        .json(&json!({ "title": "Mine" }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .delete(&format!("/v1/instructions/{id}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
}
