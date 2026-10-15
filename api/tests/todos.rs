mod common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

fn texts(list: &Value) -> Vec<&str> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|t| t["rich_text"].as_str().unwrap())
        .collect()
}

#[test_db::test(seed)]
async fn the_todo_list_keeps_its_order(pool: MySqlPool) {
    let server = server(pool);
    let add = |text: &str| {
        let request = server
            .post("/v1/me/todos")
            .with_session(sessions::REP)
            .json(&json!({ "rich_text": text }));
        async move { request.await.json::<Value>()["id"].as_i64().unwrap() }
    };
    let list = || {
        let request = server.get("/v1/me/todos").with_session(sessions::REP);
        async move { request.await.json::<Value>() }
    };
    let first = add("Call Alice").await;
    let second = add("Order slab").await;
    let third = add("Measure").await;
    // New ones go on top.
    assert_eq!(
        texts(&list().await),
        ["Measure", "Order slab", "Call Alice"]
    );

    // Ticking moves to the bottom; unticking back above the ticked ones.
    server
        .put(&format!("/v1/me/todos/{third}/done"))
        .with_session(sessions::REP)
        .json(&json!({ "done": true }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let after = list().await;
    assert_eq!(texts(&after), ["Order slab", "Call Alice", "Measure"]);
    assert_eq!(after[2]["is_done"], true);

    server
        .put("/v1/me/todo-order")
        .with_session(sessions::REP)
        .json(&json!([first, second, third]))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        texts(&list().await),
        ["Call Alice", "Order slab", "Measure"]
    );

    server
        .put(&format!("/v1/me/todos/{first}"))
        .with_session(sessions::REP)
        .json(&json!({ "rich_text": "Call Alice back" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .delete("/v1/me/todos/done")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(texts(&list().await), ["Call Alice back", "Order slab"]);

    // Someone else's to-do is out of reach.
    server
        .put(&format!("/v1/me/todos/{second}"))
        .with_session(sessions::MANAGER)
        .json(&json!({ "rich_text": "x" }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .delete(&format!("/v1/me/todos/{second}"))
        .with_session(sessions::MANAGER)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(texts(&list().await), ["Call Alice back", "Order slab"]);
    server
        .post("/v1/me/todos")
        .with_session(sessions::REP)
        .json(&json!({ "rich_text": "" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}
