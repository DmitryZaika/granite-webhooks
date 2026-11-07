//! Contact groups in the app's Contacts (`routes/phone/contact_groups.rs`):
//! the company's groups, shared by its users, and each user's own hidden
//! list (Coworkers included).
//!
//! Fixture phones use n = 401..=410.

use crate::phone_common;

use axum::http::{Method, StatusCode};
use phone_common::{
    HEADER, add_session, assert_device_only, phone_server, revoke_session, sql, token,
};
use serde_json::{Value, json};
use sqlx::MySqlPool;

/// Rita (100) and Mark (101) of company 100, Olga (106) of company 101.
const RITA: u32 = 401;
const MARK: u32 = 402;
const OLGA: u32 = 403;

async fn sessions(pool: &MySqlPool) {
    add_session(pool, RITA, 100, 100).await;
    add_session(pool, MARK, 101, 100).await;
    add_session(pool, OLGA, 106, 101).await;
}

async fn list(server: &axum_test::TestServer, n: u32) -> Value {
    let res = server
        .get("/v1/phone/contact-groups")
        .add_header(HEADER, token(n))
        .await;
    res.assert_status_ok();
    res.json()
}

#[test_db::test(seed)]
async fn the_routes_take_only_the_device_token(pool: MySqlPool) {
    add_session(&pool, 404, 100, 100).await;
    revoke_session(&pool, 404).await;
    let (server, _) = phone_server(pool.clone()).await;
    assert_device_only(
        &server,
        404,
        &[
            (Method::GET, "/v1/phone/contact-groups".into()),
            (Method::POST, "/v1/phone/contact-groups".into()),
            (Method::PATCH, "/v1/phone/contact-groups/1".into()),
            (Method::DELETE, "/v1/phone/contact-groups/1".into()),
            (Method::PUT, "/v1/phone/contact-groups/1/members".into()),
            (Method::PATCH, "/v1/phone/contact-groups/coworkers".into()),
        ],
    )
    .await;
}

#[test_db::test(seed)]
#[allow(clippy::too_many_lines)]
async fn groups_are_the_companys_and_hiding_is_mine(pool: MySqlPool) {
    sessions(&pool).await;
    let (server, _) = phone_server(pool.clone()).await;
    assert_eq!(
        list(&server, RITA).await,
        json!({ "coworkersHidden": false, "groups": [] })
    );

    // Rita makes a group with customers and a coworker; repeats count once.
    let res = server
        .post("/v1/phone/contact-groups")
        .add_header(HEADER, token(RITA))
        .json(&json!({ "name": "  Installers ", "customerIds": [1004, 1000, 1004],
                       "userIds": [101] }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let group: Value = res.json();
    let id = group["id"].as_i64().unwrap();
    let installers = json!({ "id": id, "name": "Installers", "hidden": false,
                             "customerIds": [1000, 1004], "userIds": [101], "createdBy": 100 });
    assert_eq!(group, installers);
    let (made_by,): (Option<i32>,) =
        sqlx::query_as("SELECT created_by FROM phone_contact_groups WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(made_by, Some(100));

    // Mark (same company) sees it; Olga (another company) does not.
    assert_eq!(
        list(&server, MARK).await,
        json!({ "coworkersHidden": false, "groups": [installers] })
    );
    assert_eq!(list(&server, OLGA).await["groups"], json!([]));

    // Mark hides it and Coworkers: only from his own top.
    let res = server
        .patch(&format!("/v1/phone/contact-groups/{id}"))
        .add_header(HEADER, token(MARK))
        .json(&json!({ "hidden": true }))
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["hidden"], true);
    server
        .patch("/v1/phone/contact-groups/coworkers")
        .add_header(HEADER, token(MARK))
        .json(&json!({ "hidden": true }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    // Twice is the same.
    server
        .patch("/v1/phone/contact-groups/coworkers")
        .add_header(HEADER, token(MARK))
        .json(&json!({ "hidden": true }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let marks = list(&server, MARK).await;
    assert_eq!(marks["coworkersHidden"], true);
    assert_eq!(marks["groups"][0]["hidden"], true);
    assert_eq!(marks["groups"][0]["customerIds"], json!([1000, 1004]));
    assert_eq!(
        list(&server, RITA).await,
        json!({ "coworkersHidden": false, "groups": [installers] })
    );

    // Mark renames it for everyone; the name is checked.
    let res = server
        .patch(&format!("/v1/phone/contact-groups/{id}"))
        .add_header(HEADER, token(MARK))
        .json(&json!({ "name": "Crew" }))
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["name"], "Crew");
    assert_eq!(list(&server, RITA).await["groups"][0]["name"], "Crew");
    for (body, error) in [
        (json!({ "name": "crew " }), Some("name_taken")),
        (json!({ "name": " " }), None),
        (json!({ "name": "x".repeat(61) }), None),
    ] {
        let res = server
            .post("/v1/phone/contact-groups")
            .add_header(HEADER, token(RITA))
            .json(&body)
            .await;
        res.assert_status(StatusCode::BAD_REQUEST);
        if let Some(error) = error {
            assert_eq!(res.json::<Value>()["error"], error, "{body}");
        }
    }
    // Its own name again (another case) is fine; nothing to change is not.
    server
        .patch(&format!("/v1/phone/contact-groups/{id}"))
        .add_header(HEADER, token(RITA))
        .json(&json!({ "name": "CREW" }))
        .await
        .assert_status_ok();
    server
        .patch(&format!("/v1/phone/contact-groups/{id}"))
        .add_header(HEADER, token(RITA))
        .json(&json!({}))
        .await
        .assert_status(StatusCode::BAD_REQUEST);

    // Members: exactly what is sent, checked against the company.
    let res = server
        .put(&format!("/v1/phone/contact-groups/{id}/members"))
        .add_header(HEADER, token(RITA))
        .json(&json!({ "customerIds": [1010, 1004], "userIds": [100, 101] }))
        .await;
    res.assert_status_ok();
    let group: Value = res.json();
    assert_eq!(group["customerIds"], json!([1004, 1010]));
    assert_eq!(group["userIds"], json!([100, 101]));
    // Who stayed keeps who added them.
    let (added_by,): (Option<i32>,) = sqlx::query_as(
        "SELECT added_by FROM phone_contact_group_members WHERE group_id = ? AND customer_id = 1004",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(added_by, Some(100));
    for (body, error) in [
        (json!({ "customerIds": [2000] }), "unknown_contact"),
        (json!({ "customerIds": [1003] }), "unknown_contact"),
        (json!({ "userIds": [106] }), "unknown_coworker"),
        (json!({ "userIds": [105] }), "unknown_coworker"),
        (
            json!({ "customerIds": (1..=501).collect::<Vec<i32>>() }),
            "too_many_members",
        ),
    ] {
        let res = server
            .put(&format!("/v1/phone/contact-groups/{id}/members"))
            .add_header(HEADER, token(MARK))
            .json(&body)
            .await;
        res.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(res.json::<Value>()["error"], error, "{body}");
    }
    // A customer deleted later is left out.
    sql(
        &pool,
        "UPDATE customers SET deleted_at = UTC_TIMESTAMP() WHERE id = 1010",
    )
    .await;
    assert_eq!(
        list(&server, MARK).await["groups"][0]["customerIds"],
        json!([1004])
    );

    // Another company's user can't touch it.
    for request in [
        server
            .patch(&format!("/v1/phone/contact-groups/{id}"))
            .json(&json!({ "name": "Mine" })),
        server
            .put(&format!("/v1/phone/contact-groups/{id}/members"))
            .json(&json!({ "customerIds": [] })),
        server
            .delete(&format!("/v1/phone/contact-groups/{id}"))
            .json(&json!({})),
    ] {
        request
            .add_header(HEADER, token(OLGA))
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }

    // Mark deletes it for everyone (with his hidden mark); Coworkers stays hidden for him.
    server
        .delete(&format!("/v1/phone/contact-groups/{id}"))
        .add_header(HEADER, token(MARK))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(list(&server, RITA).await["groups"], json!([]));
    assert_eq!(
        list(&server, MARK).await,
        json!({ "coworkersHidden": true, "groups": [] })
    );
    let (left,): (i64,) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM phone_contact_group_members WHERE group_id = ?)
              + (SELECT COUNT(*) FROM phone_contact_group_hidden WHERE group_id = ?)",
    )
    .bind(id)
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(left, 0);
    server
        .delete(&format!("/v1/phone/contact-groups/{id}"))
        .add_header(HEADER, token(MARK))
        .await
        .assert_status(StatusCode::NOT_FOUND);

    // Shown again.
    server
        .patch("/v1/phone/contact-groups/coworkers")
        .add_header(HEADER, token(MARK))
        .json(&json!({ "hidden": false }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(list(&server, MARK).await["coworkersHidden"], false);
}

#[test_db::test(seed)]
async fn groups_are_listed_by_name_and_limited(pool: MySqlPool) {
    sessions(&pool).await;
    let (server, _) = phone_server(pool.clone()).await;
    for name in ["builders", "Architects", "Crew"] {
        server
            .post("/v1/phone/contact-groups")
            .add_header(HEADER, token(RITA))
            .json(&json!({ "name": name }))
            .await
            .assert_status(StatusCode::CREATED);
    }
    let names: Vec<Value> = list(&server, MARK).await["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|group| group["name"].clone())
        .collect();
    assert_eq!(names, [json!("Architects"), json!("builders"), json!("Crew")]);
    // Another company may use the same name.
    server
        .post("/v1/phone/contact-groups")
        .add_header(HEADER, token(OLGA))
        .json(&json!({ "name": "Crew" }))
        .await
        .assert_status(StatusCode::CREATED);
    // At most 100 groups a company.
    sql(
        &pool,
        "INSERT INTO phone_contact_groups (company_id, name)
         WITH RECURSIVE n (i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 97)
         SELECT 100, CONCAT('Group ', i) FROM n",
    )
    .await;
    let res = server
        .post("/v1/phone/contact-groups")
        .add_header(HEADER, token(RITA))
        .json(&json!({ "name": "One more" }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(res.json::<Value>()["error"], "too_many_groups");
}
