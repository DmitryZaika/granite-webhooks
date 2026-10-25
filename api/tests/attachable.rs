use crate::common;

use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

fn names(list: &Value) -> Vec<&str> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|item| item["name"].as_str().unwrap())
        .collect()
}

#[test_db::test(seed)]
async fn stones_to_attach_have_an_image_and_stock(pool: MySqlPool) {
    let server = server(pool);
    let all: Value = server
        .get("/v1/stones/attachable")
        .with_session(sessions::REP)
        .await
        .json();
    // Displayed, in stock, with an image (the page hides the tour's sample).
    assert_eq!(
        names(&all),
        [
            "Absolute Black",
            "Calacatta Gold",
            "Linked Black Remnants",
            "Onboarding Sample Granite",
            "Regular Stock Quartzite",
            "Zebra Quartz"
        ]
    );
    let black = all
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == 3000)
        .unwrap();
    assert_eq!(black["installed_count"], 2);
    assert_eq!(black["type"], "granite");

    let filtered: Value = server
        .get("/v1/stones/attachable?name=black&type=granite")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(
        names(&filtered),
        ["Absolute Black", "Linked Black Remnants"]
    );
}

#[test_db::test(seed)]
async fn the_library_lists_the_companys_files(pool: MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO images (id, name, url, company_id) VALUES (9800, 'Kitchen', 'https://i/k.jpg', 100), (9801, 'Bath', 'https://i/b.jpg', 101);
         INSERT INTO documents (id, name, url, company_id) VALUES (9810, 'Warranty', 'https://d/w.pdf', 100);",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool);
    let images: Value = server
        .get("/v1/images")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(
        images,
        json!([{ "id": 9800, "name": "Kitchen", "url": "https://i/k.jpg" }])
    );
    let none: Value = server
        .get("/v1/images?name=bath")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(none, json!([]));
    let documents: Value = server
        .get("/v1/documents?name=warr")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(names(&documents), ["Warranty"]);
}
