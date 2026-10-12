mod common;

use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

#[test_db::test(seed)]
async fn a_layout_sees_free_slabs_of_the_stone_and_its_links(pool: MySqlPool) {
    let server = server(pool);
    let slabs = |query: &str| {
        let request = server
            .get(&format!("/v1/quotes/inventory-slabs?{query}"))
            .with_session(sessions::REP);
        async move { request.await.json::<Value>() }
    };
    let black = slabs("name=%20absolute%20black%20&type=GRANITE").await;
    assert_eq!(black["stone_url"], "https://img.example/absolute-black.jpg");
    let ids: Vec<i64> = black["slabs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, [6000, 6001, 6002]);
    // The linked stone shows Absolute Black's free slabs.
    let linked = slabs("name=Linked%20Black%20Remnants").await;
    assert_eq!(linked["slabs"].as_array().unwrap().len(), 3);
    assert_eq!(
        slabs("name=Absolute%20Black&type=quartz").await["slabs"],
        json!([])
    );
    assert_eq!(
        slabs("name=").await,
        json!({ "stone_url": null, "slabs": [] })
    );
}
