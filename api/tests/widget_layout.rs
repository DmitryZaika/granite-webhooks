use crate::common;

use axum::http::StatusCode;
use common::{cookie, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// React Router stored `"[\"deals_by_rep\"]"` as `IltcImRlYWxzX2J5X3JlcFwiXSI%3D`.
const SAVED: &str = "IltcImRlYWxzX2J5X3JlcFwiXSI%3D";

#[test_db::test(seed)]
async fn the_layout_is_read_from_and_saved_to_the_browser_cookie(pool: MySqlPool) {
    sqlx::query("UPDATE company SET statistics_widgets_hidden = 1 WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();
    let server = server(pool);
    let session = cookie(sessions::ADMIN);
    let empty: Value = server
        .get("/v1/stats/widget-layout")
        .add_header("cookie", session.clone())
        .await
        .json();
    assert_eq!(empty, json!({ "saved": null, "hidden_by_default": true }));
    let saved: Value = server
        .get("/v1/stats/widget-layout")
        .add_header(
            "cookie",
            format!("{session}; admin-statistics-widgets-100={SAVED}"),
        )
        .await
        .json();
    assert_eq!(saved["saved"], "[\"deals_by_rep\"]");
    // Another company's cookie is not this one.
    let other: Value = server
        .get("/v1/stats/widget-layout")
        .add_header(
            "cookie",
            format!("{session}; admin-statistics-widgets-101={SAVED}"),
        )
        .await
        .json();
    assert_eq!(other["saved"], Value::Null);

    let res = server
        .put("/v1/stats/widget-layout")
        .add_header("cookie", session.clone())
        .add_header("origin", common::ALLOWED_ORIGIN)
        .json(&json!({ "value": "[\"deals_by_rep\"]" }))
        .await;
    res.assert_status(StatusCode::NO_CONTENT);
    let set = res.header("set-cookie");
    let set = set.to_str().unwrap();
    assert!(
        set.starts_with(&format!("admin-statistics-widgets-100={SAVED};")),
        "{set}"
    );
    assert!(set.contains("HttpOnly") && set.contains("Max-Age=31536000"));

    let too_big = "x".repeat(4000);
    server
        .put("/v1/stats/widget-layout")
        .add_header("cookie", session.clone())
        .add_header("origin", common::ALLOWED_ORIGIN)
        .json(&json!({ "value": too_big }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .get("/v1/stats/widget-layout")
        .add_header("cookie", cookie(sessions::REP))
        .await
        .assert_status(StatusCode::FORBIDDEN);
}
