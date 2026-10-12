mod common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Seed customers 1000-1011 of company 100 (1003 deleted), 2000 of company 101.

fn search_ids(rows: &Value) -> Vec<i64> {
    common::ids(rows)
}

async fn search(pool: MySqlPool, query: &str) -> Value {
    let res = server(pool)
        .get(&format!("/v1/customers/search{query}"))
        .with_session(sessions::REP)
        .await;
    res.assert_status_ok();
    res.json()
}

#[test_db::test(seed)]
async fn get_returns_contact_details_and_every_email(pool: MySqlPool) {
    let res = server(pool)
        .get("/v1/customers/1000")
        .with_session(sessions::REP)
        .await;
    res.assert_status_ok();
    assert_eq!(
        res.json::<Value>(),
        json!({
            "id": 1000,
            "name": "Alice Lead",
            "address": "1 Main St",
            "phone": "317-555-0100",
            "phone_2": "317-555-0101",
            "email": "alice@example.com",
            "emails": ["alice@example.com", "alice.work@example.com", "alice.old@example.com"],
            "company_name": null,
            "source": "leads",
            "referral_source": null,
            "your_message": null,
            "sales_rep": 100,
            "sales_rep_name": "Rita Rep",
            "city": null,
            "state": null,
            "postal_code": null,
            "customer_temperature": "hot",
            "notes": null,
            "qbo_id": null,
            "attached_file": null,
            "created_date": "2026-01-15T10:00:00.000Z",
            "invalid_lead": null,
            "lead": {
                "remodal_type": null, "project_size": null, "contact_time": null,
                "remove_and_dispose": null, "improve_offer": null, "sink": null,
                "when_start": null, "details": null, "compaign_name": null,
                "adset_name": null, "ad_name": null, "backsplash": null, "kitchen_stove": null
            }
        })
    );
}

#[test_db::test(seed)]
async fn get_returns_the_lead_form_answers(pool: MySqlPool) {
    sqlx::query(
        "UPDATE customers SET city = 'Carmel', state = 'IN', postal_code = '46032',
           remodal_type = 'Kitchen', improve_offer = 1, ad_name = 'Spring', attached_file = 'https://x.test/a.jpg'
         WHERE id = 1004",
    )
    .execute(&pool)
    .await
    .unwrap();
    let res = server(pool)
        .get("/v1/customers/1004")
        .with_session(sessions::REP)
        .await;
    let body: Value = res.json();
    assert_eq!(body["city"], "Carmel");
    assert_eq!(body["postal_code"], "46032");
    assert_eq!(body["attached_file"], "https://x.test/a.jpg");
    assert_eq!(body["customer_temperature"], "medium");
    assert_eq!(body["lead"]["remodal_type"], "Kitchen");
    assert_eq!(body["lead"]["improve_offer"], 1);
    assert_eq!(body["lead"]["ad_name"], "Spring");
}

#[test_db::test(seed)]
async fn get_hides_deleted_reps_and_foreign_or_deleted_customers(pool: MySqlPool) {
    let server = server(pool);
    let gina: Value = server
        .get("/v1/customers/1006")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(gina["sales_rep"], 105);
    assert_eq!(gina["sales_rep_name"], Value::Null);
    assert_eq!(gina["emails"], json!([]));
    for id in [1003, 2000, 999_999] {
        server
            .get(&format!("/v1/customers/{id}"))
            .with_session(sessions::REP)
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }
    server
        .get("/v1/customers/1000")
        .await
        .assert_status_unauthorized();
}

#[test_db::test(seed)]
async fn name_search_tolerates_typos_after_three_letters(pool: MySqlPool) {
    for (query, expected) in [
        ("?term=Alice", vec![1000]),
        ("?term=alice%20lead", vec![1000]),
        ("?term=Alic%20Lea", vec![1000]),
        // Each word must share its first three letters with the name.
        ("?term=Alic%20Led", vec![]),
        // Company names match a name search too.
        ("?term=Builders", vec![1004]),
        ("?term=Zo", vec![1010]),
        ("?term=Dan", vec![]),
        ("?term=Other", vec![]),
    ] {
        let rows = search(pool.clone(), query).await;
        assert_eq!(search_ids(&rows), expected, "{query}");
    }
    let rows = search(pool, "?term=Alice").await;
    assert_eq!(
        rows[0],
        json!({
            "id": 1000,
            "name": "Alice Lead",
            "address": "1 Main St",
            "phone": "317-555-0100",
            "phone_2": "317-555-0101",
            "email": "alice@example.com",
            "emails": ["alice@example.com", "alice.work@example.com", "alice.old@example.com"],
            "company_name": null,
            // Her newest deal (dashboard seed): 8007, open, in group 100.
            "deal_id": 8007,
            "deal_is_won": null,
            "deal_group_id": 100
        })
    );
}

#[test_db::test(seed)]
async fn own_customers_come_first(pool: MySqlPool) {
    // "er": Erin Builder (rep 101) and Rita's... none; use "a" for a wide match.
    let rows = search(pool.clone(), "?term=a").await;
    let ids = search_ids(&rows);
    // Rita's own customers (Alice, Carol, Ivy, Jack) before everyone else.
    let first_other = ids
        .iter()
        .position(|id| ![1000, 1002, 1008, 1011].contains(id))
        .unwrap();
    assert!(
        ids[..first_other]
            .iter()
            .all(|id| [1000, 1002, 1008, 1011].contains(id)),
        "{ids:?}"
    );
    assert!(ids.len() <= 15);
    let own = search(pool, "?term=a&own_only=true").await;
    for row in own.as_array().unwrap() {
        let id = row["id"].as_i64().unwrap();
        assert!(
            ![1004, 1005, 1006, 1010].contains(&id),
            "{id} belongs to another rep"
        );
    }
}

#[test_db::test(seed)]
async fn phone_email_and_company_search(pool: MySqlPool) {
    for (query, expected) in [
        ("?by=phone&term=(317)%20555-0105", vec![1004]),
        ("?by=phone&term=3175550101", vec![1000]),
        ("?by=phone&term=%2B1%20317%20555%200110", vec![1010]),
        ("?by=email&term=jack%40", vec![1011]),
        ("?by=email&term=alice.work", vec![1000]),
        ("?by=company&term=Erin", vec![1004]),
        ("?by=company&term=Hank", vec![1007]),
        ("?by=company&term=Alice", vec![]),
        // Every word somewhere in the address.
        ("?by=address&term=oak%205", vec![1004]),
        ("?by=address&term=main", vec![1000]),
        ("?by=address&term=nowhere", vec![]),
    ] {
        let rows = search(pool.clone(), query).await;
        assert_eq!(search_ids(&rows), expected, "{query}");
    }
}

#[test_db::test(seed)]
async fn search_validates_and_authenticates(pool: MySqlPool) {
    let server = server(pool);
    for query in ["", "?term=a&by=fax"] {
        server
            .get(&format!("/v1/customers/search{query}"))
            .with_session(sessions::REP)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    server
        .get("/v1/customers/search?term=a")
        .await
        .assert_status_unauthorized();
    let other: Value = server
        .get("/v1/customers/search?term=Other")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .json();
    assert_eq!(search_ids(&other), vec![2000]);
}

#[test_db::test(seed)]
async fn deleted_customers_only_on_request(pool: MySqlPool) {
    let app = server(pool);
    app.get("/v1/customers/1003")
        .with_session(sessions::REP)
        .await
        .assert_status_not_found();
    let res = app
        .get("/v1/customers/1003?include_deleted=true")
        .with_session(sessions::REP)
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["name"], "Dan Deleted");
    // Never another company's.
    app.get("/v1/customers/2000?include_deleted=true")
        .with_session(sessions::REP)
        .await
        .assert_status_not_found();
}

#[test_db::test(seed)]
async fn duplicates_match_phones_and_addresses(pool: MySqlPool) {
    let server = server(pool);
    let find = |query: &'static str| {
        let server = &server;
        async move {
            let res = server
                .get(&format!("/v1/customers/duplicates{query}"))
                .with_session(sessions::REP)
                .await;
            res.assert_status_ok();
            res.json::<Value>()
        }
    };
    let by_phone = find("?phone=%2B1%20(317)%20555-0100").await;
    assert_eq!(by_phone[0]["id"], 1000);
    assert_eq!(by_phone[0]["sales_rep_name"], "Rita Rep");
    assert_eq!(find("?phone_2=317.555.0101").await[0]["id"], 1000);
    assert_eq!(
        find("?email=nobody%40x.test&email=ALICE.WORK%40example.com").await[0]["id"],
        1000
    );
    assert_eq!(find("?phone=555").await, serde_json::json!([]));
    assert_eq!(find("").await, serde_json::json!([]));
    // Another company's customer is not a duplicate here.
    assert_eq!(find("?phone=463-555-0200").await, serde_json::json!([]));
}
