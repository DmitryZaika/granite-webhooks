//! Quote price lists and the extras catalog (Admin > Quotes).

mod common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

async fn create(server: &axum_test::TestServer, name: &str) -> i64 {
    let res = server
        .post("/v1/quote-price-lists")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": name }))
        .await;
    res.assert_status(StatusCode::CREATED);
    res.json::<Value>()["id"].as_i64().unwrap()
}

async fn editor(server: &axum_test::TestServer, id: i64) -> Value {
    let res = server
        .get(&format!("/v1/quote-price-lists/{id}"))
        .with_session(sessions::ADMIN)
        .await;
    res.assert_status_ok();
    res.json::<Value>()
}

#[test_db::test(seed)]
async fn admins_create_save_and_restore_price_lists(pool: MySqlPool) {
    let server = server(pool.clone());
    let id = create(&server, "Retail 2027").await;
    let res = server
        .post("/v1/quote-price-lists")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": " Retail 2027 " }))
        .await;
    res.assert_status(StatusCode::CONFLICT);
    assert_eq!(
        res.json::<Value>()["error"],
        "\"Retail 2027\" already exists."
    );

    let list = server
        .get("/v1/quote-price-lists")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(list["default_id"], id);
    assert_eq!(list["price_lists"][0]["revision_number"], 1);
    assert_eq!(list["price_lists"][0]["units"], "in");
    assert_eq!(list["price_lists"][0]["expiration_days"], 30);
    assert_eq!(list["price_lists"][0]["created_by_name"], "Ada Admin");

    let opened = editor(&server, id).await;
    assert_eq!(opened["revisions"][0]["notes"], "Created");
    assert!(opened["stock"]["stones"].is_array());
    let mut info = opened["synced_info"].clone();
    info["expirationDays"] = json!(45);
    info["arrOtherItems"] = json!([{ "name": " Haul away ", "category": " " }]);
    server
        .put(&format!("/v1/quote-price-lists/{id}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Retail 2027", "status": "active", "info": info, "notes": "Longer quotes" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let saved = editor(&server, id).await;
    assert_eq!(saved["price_list"]["revision_number"], 2);
    assert_eq!(saved["info"]["expirationDays"], 45);
    assert_eq!(saved["info"]["revision"], 2);
    assert_eq!(
        saved["info"]["arrOtherItems"],
        json!([{ "name": "Haul away" }])
    );
    assert_eq!(saved["revisions"][0]["notes"], "Longer quotes");

    // Ambiguous prices are refused with every problem.
    let mut bad = saved["info"].clone();
    bad["arrOtherItems"] = json!([{ "name": "A" }, { "name": "A" }]);
    let res = server
        .put(&format!("/v1/quote-price-lists/{id}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Retail 2027", "status": "active", "info": bad }))
        .await;
    res.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        res.json::<Value>()["errors"],
        json!(["Extra \"A\" is listed more than once."])
    );

    let first = saved["revisions"][1]["id"].as_i64().unwrap();
    let res = server
        .post(&format!(
            "/v1/quote-price-lists/{id}/revisions/{first}/restore"
        ))
        .with_session(sessions::ADMIN)
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["revision_number"], 1);
    let restored = editor(&server, id).await;
    assert_eq!(restored["price_list"]["revision_number"], 3);
    assert_eq!(restored["info"]["expirationDays"], 30);
    assert_eq!(restored["revisions"][0]["notes"], "Restored revision 1");

    // Reps read the lists but cannot change them.
    server
        .post("/v1/quote-price-lists")
        .with_session(sessions::REP)
        .json(&json!({ "name": "Mine" }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .get(&format!("/v1/quote-price-lists/{id}"))
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn admins_copy_star_rename_deactivate_and_delete(pool: MySqlPool) {
    let server = server(pool.clone());
    let first = create(&server, "Retail").await;
    let res = server
        .post(&format!("/v1/quote-price-lists/{first}/copies"))
        .with_session(sessions::ADMIN)
        .await;
    res.assert_status(StatusCode::CREATED);
    let copy = res.json::<Value>()["id"].as_i64().unwrap();
    assert_eq!(
        editor(&server, copy).await["price_list"]["name"],
        "Retail (copy)"
    );

    server
        .put("/v1/quote-price-lists/default")
        .with_session(sessions::ADMIN)
        .json(&json!({ "id": copy }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .patch(&format!("/v1/quote-price-lists/{first}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "status": "inactive" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let res = server
        .patch(&format!("/v1/quote-price-lists/{first}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Retail (copy)" }))
        .await;
    res.assert_status(StatusCode::CONFLICT);
    assert_eq!(
        res.json::<Value>()["error"],
        "Another price list is already called \"Retail (copy)\"."
    );
    let list = server
        .get("/v1/quote-price-lists")
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>();
    assert_eq!(list["default_id"], copy);
    assert_eq!(list["price_lists"][1]["status"], "inactive");

    server
        .delete(&format!("/v1/quote-price-lists/{first}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .get(&format!("/v1/quote-price-lists/{first}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .put("/v1/quote-price-lists/default")
        .with_session(sessions::ADMIN)
        .json(&json!({ "id": first }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn the_extras_catalog_is_added_to_saved_and_filled_from_the_lists(pool: MySqlPool) {
    let server = server(pool.clone());
    let a = create(&server, "Retail").await;
    let b = create(&server, "Builder").await;
    // List A still keeps an extra of its own.
    sqlx::query(
        "UPDATE price_list_revisions r JOIN price_lists pl ON pl.current_revision_id = r.id
            SET r.info_json = JSON_SET(r.info_json, '$.arrOtherItems',
                JSON_ARRAY(JSON_OBJECT('name', 'Haul away', 'arrRanges', JSON_ARRAY(JSON_OBJECT('flatPrice', 75)))))
          WHERE pl.id = ?",
    )
    .bind(a)
    .execute(&pool)
    .await
    .unwrap();

    let res = server
        .post("/v1/quote-extras")
        .with_session(sessions::ADMIN)
        .json(&json!({ "item": { "name": "Tear out", "arrOptions": [{ "name": "Laminate", "flatPrice": 100 }] }, "off_list_ids": [b] }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let tear_out = res.json::<Value>()["id"].as_i64().unwrap();
    server
        .post("/v1/quote-extras")
        .with_session(sessions::ADMIN)
        .json(&json!({ "item": { "name": "tear OUT" } }))
        .await
        .assert_status(StatusCode::CONFLICT);
    let builder = editor(&server, b).await;
    assert_eq!(
        builder["info"]["extraLinks"][tear_out.to_string()],
        json!({ "off": true })
    );
    assert_eq!(
        builder["revisions"][0]["notes"],
        "Extra \"Tear out\" not offered"
    );

    let catalog = server
        .get("/v1/quote-extras")
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>();
    assert_eq!(catalog["list_only"], 1);
    assert!(
        catalog["catalog"][0]["item"]["arrOptions"][0]["id"]
            .as_str()
            .unwrap()
            .starts_with("opt-")
    );

    let res = server
        .post("/v1/quote-extras/moves")
        .with_session(sessions::ADMIN)
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["created"], 1);
    let catalog = server
        .get("/v1/quote-extras")
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>();
    assert_eq!(catalog["list_only"], 0);
    assert_eq!(catalog["catalog"][1]["item"]["name"], "Haul away");
    let haul = catalog["catalog"][1]["id"].as_i64().unwrap();
    // The other list never had it: switched off there.
    assert_eq!(
        editor(&server, b).await["info"]["extraLinks"][haul.to_string()],
        json!({ "off": true })
    );

    // Saving the catalog without Tear out deletes it; names stay unique.
    let mut kept = catalog["catalog"][1].clone();
    kept["item"]["name"] = json!("Haul off");
    server
        .put("/v1/quote-extras")
        .with_session(sessions::ADMIN)
        .json(&json!({ "extras": [kept.clone(), kept.clone()] }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .put("/v1/quote-extras")
        .with_session(sessions::ADMIN)
        .json(&json!({ "extras": [kept] }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let catalog = server
        .get("/v1/quote-extras")
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>();
    assert_eq!(catalog["catalog"].as_array().unwrap().len(), 1);
    assert_eq!(catalog["catalog"][0]["item"]["name"], "Haul off");
}
