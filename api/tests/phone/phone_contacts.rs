//! Granite Phone contacts: the offline index (pages, since-sync, ETag),
//! lookups, the contact card, new customers and added numbers
//! (`routes/phone/contacts.rs`).
//!
//! Fixture phones use n = 41..=60; numbers 9720+; customers 930000+; calls
//! 980100+.

use crate::phone_common;

use axum::http::{Method, StatusCode};
use chrono::Utc;
use phone_common::{
    APP_URL, HEADER, add_call, add_number, add_phone, add_session, assert_device_only, assign,
    phone_server, revoke_session, sql, token,
};
use serde_json::{Value, json};
use sqlx::MySqlPool;

/// Direct lines: Mark's and Dina's (no label) are coworkers; Rita's own,
/// Olga's (another company's user), a deleted user's and a shared line are
/// not. (No seeded user has another number.)
async fn coworker_lines(pool: &MySqlPool) {
    add_number(pool, 9721, 100, "+13175550121", "direct", "Mark direct").await;
    add_number(pool, 9722, 100, "+13175550122", "direct", "").await;
    add_number(pool, 9723, 100, "+13175550123", "direct", "Rita direct").await;
    add_number(pool, 9724, 100, "+13175550124", "direct", "Olga?").await;
    add_number(pool, 9725, 100, "+13175550125", "direct", "Gus").await;
    add_number(pool, 9726, 100, "+13175550126", "shared", "Sales").await;
    assign(pool, 9721, 101, true, true).await;
    assign(pool, 9722, 107, false, true).await;
    assign(pool, 9723, 100, true, true).await;
    assign(pool, 9724, 106, true, true).await;
    assign(pool, 9725, 105, true, true).await;
    assign(pool, 9726, 102, true, true).await;
}

fn coworkers() -> Value {
    json!([
        { "userId": 107, "name": "Dina Dealer", "e164": "+13175550122", "label": null,
          "phones": [{ "e164": "+13175550122", "label": null }] },
        { "userId": 101, "name": "Mark Manager", "e164": "+13175550121", "label": "Mark direct",
          "phones": [{ "e164": "+13175550121", "label": "Mark direct" }] },
    ])
}

fn ids(contacts: &Value) -> Vec<i64> {
    contacts
        .as_array()
        .unwrap()
        .iter()
        .map(|contact| contact["id"].as_i64().unwrap())
        .collect()
}

fn find(contacts: &Value, id: i64) -> Value {
    contacts
        .as_array()
        .unwrap()
        .iter()
        .find(|contact| contact["id"] == id)
        .cloned()
        .unwrap_or_else(|| panic!("contact {id} missing"))
}

#[test_db::test(seed)]
async fn the_index_has_the_companys_customers_and_coworkers(pool: MySqlPool) {
    coworker_lines(&pool).await;
    add_call(
        &pool,
        980101,
        100,
        Some(101),
        "inbound",
        "answered",
        "+13175550100",
        "2026-10-09 14:00:00",
        "customer_id = 1000",
    )
    .await;
    add_call(
        &pool,
        980102,
        100,
        Some(100),
        "outbound",
        "no_answer",
        "+13175550100",
        "2026-10-10 15:00:00.250",
        "customer_id = 1000",
    )
    .await;
    // Another company's call with Alice's id must not count.
    add_call(
        &pool,
        980103,
        101,
        Some(106),
        "outbound",
        "answered",
        "+13175550100",
        "2026-10-11 15:00:00",
        "customer_id = 1000",
    )
    .await;
    add_session(&pool, 41, 100, 100).await;
    let (server, _) = phone_server(pool.clone()).await;
    let response = server
        .get("/v1/phone/contacts/index")
        .add_header(HEADER, token(41))
        .await;
    response.assert_status_ok();
    let etag = response.headers()["etag"].to_str().unwrap().to_string();
    assert!(
        etag.len() == 34 && etag.starts_with('"') && etag.ends_with('"'),
        "{etag}"
    );
    assert_eq!(response.headers()["cache-control"], "private, no-cache");
    let body: Value = response.json();
    let cursor = body["cursor"].as_str().unwrap().to_string();
    // Live customers by id: not the deleted one (1003), the onboarding
    // sample (990000021) or company 101's (2000).
    assert_eq!(
        ids(&body["contacts"]),
        [
            1000, 1001, 1002, 1004, 1005, 1006, 1007, 1008, 1009, 1010, 1011, 8695
        ]
    );
    assert_eq!(
        (
            &body["full"],
            &body["deleted"],
            &body["nextAfter"],
            &body["coworkers"]
        ),
        (&json!(true), &json!([]), &Value::Null, &coworkers())
    );
    assert!(cursor.contains('.'));
    // Alice: two phones, Rita's (mine), hot, her oldest open deal (in a
    // stage that was archived), the last call.
    assert_eq!(
        find(&body["contacts"], 1000),
        json!({ "id": 1000, "name": "Alice Lead", "company": null,
                "phones": ["3175550100", "3175550101"], "repId": 100, "mine": true,
                "temperature": "hot", "openDeal": { "id": 8007, "stage": "Archived" },
                "lastCallAt": "2026-10-10T15:00:00.250Z" })
    );
    assert_eq!(
        find(&body["contacts"], 1004),
        json!({ "id": 1004, "name": "Erin Builder", "company": "Erin Builders LLC",
                "phones": ["3175550105"], "repId": 101, "mine": false,
                "temperature": "medium", "openDeal": { "id": 8002, "stage": "Quoted" },
                "lastCallAt": null })
    );
    // Empty phones are left out; no deal, no rep.
    assert_eq!(find(&body["contacts"], 1008)["phones"], json!([]));
    assert_eq!(
        find(&body["contacts"], 1002),
        json!({ "id": 1002, "name": "Carol Invalid", "company": null, "phones": ["3175550103"],
                "repId": 100, "mine": true, "temperature": "cold", "openDeal": null,
                "lastCallAt": null })
    );
    assert_eq!(find(&body["contacts"], 1001)["repId"], Value::Null);
    // The bootstrap names the same version.
    let bootstrap: Value = server
        .get("/v1/phone/bootstrap")
        .add_header(HEADER, token(41))
        .await
        .json();
    assert_eq!(bootstrap["contacts"], json!({ "count": 12, "etag": etag }));

    // Olga's index is company 101's.
    add_session(&pool, 42, 106, 101).await;
    let other: Value = server
        .get("/v1/phone/contacts/index")
        .add_header(HEADER, token(42))
        .await
        .json();
    assert_eq!(ids(&other["contacts"]), [2000]);
    assert_eq!(other["coworkers"], json!([]));
    assert_eq!(find(&other["contacts"], 2000)["mine"], true);
}

#[test_db::test(seed)]
async fn a_big_index_downloads_in_pages(pool: MySqlPool) {
    // 5,003 more customers for company 101.
    let digits = "(SELECT 0 AS d UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3
                   UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7
                   UNION ALL SELECT 8 UNION ALL SELECT 9)";
    sql(
        &pool,
        &format!(
            "INSERT INTO customers (id, name, phone, company_id, source)
             SELECT 940000 + n, CONCAT('Bulk ', n), CONCAT('463-555-', LPAD(n MOD 10000, 4, '0')),
                    101, 'leads'
               FROM (SELECT a.d + 10 * b.d + 100 * c.d + 1000 * e.d + 1 AS n
                       FROM {digits} a, {digits} b, {digits} c, {digits} e) seq
              WHERE n <= 5003"
        ),
    )
    .await;
    add_session(&pool, 43, 106, 101).await;
    let (server, _) = phone_server(pool.clone()).await;
    let first = server
        .get("/v1/phone/contacts/index")
        .add_header(HEADER, token(43))
        .await;
    first.assert_status_ok();
    let etag = first.headers()["etag"].to_str().unwrap().to_string();
    let first: Value = first.json();
    let page = ids(&first["contacts"]);
    assert_eq!(page.len(), 5000);
    assert_eq!((page[0], page[1], page[4999]), (2000, 940_001, 944_999));
    assert_eq!(first["nextAfter"], 944_999);
    assert_eq!(first["full"], true);
    // The next page, even with the ETag: the rest, and no coworkers.
    let second: Value = server
        .get("/v1/phone/contacts/index?after=944999")
        .add_header(HEADER, token(43))
        .add_header("if-none-match", etag)
        .await
        .json();
    assert_eq!(
        ids(&second["contacts"]),
        [945_000, 945_001, 945_002, 945_003]
    );
    assert_eq!(
        (
            &second["full"],
            &second["nextAfter"],
            &second["coworkers"],
            &second["deleted"]
        ),
        (&json!(true), &Value::Null, &json!([]), &json!([]))
    );
    assert_eq!(
        find(&second["contacts"], 945_003),
        json!({ "id": 945_003, "name": "Bulk 5003", "company": null, "phones": ["4635555003"],
                "repId": null, "mine": false, "temperature": null, "openDeal": null,
                "lastCallAt": null })
    );
    let bootstrap: Value = server
        .get("/v1/phone/bootstrap")
        .add_header(HEADER, token(43))
        .await
        .json();
    assert_eq!(bootstrap["contacts"]["count"], 5004);
    server
        .get("/v1/phone/contacts/index?after=nope")
        .add_header(HEADER, token(43))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

#[test_db::test(seed)]
async fn a_since_sync_has_the_changes_and_an_unchanged_index_is_304(pool: MySqlPool) {
    // The seed's own updates are long ago.
    sql(
        &pool,
        "UPDATE customers SET updated_at = '2026-01-01 00:00:00' WHERE company_id IN (100, 101)",
    )
    .await;
    add_session(&pool, 44, 100, 100).await;
    let (server, _) = phone_server(pool.clone()).await;
    let index = |query: &str| {
        server
            .get(&format!("/v1/phone/contacts/index{query}"))
            .add_header(HEADER, token(44))
    };
    let full = index("").await;
    let etag = full.headers()["etag"].to_str().unwrap().to_string();
    let cursor = full.json::<Value>()["cursor"].as_str().unwrap().to_string();
    // Unchanged: a since-sync is 304, quoted or not.
    for tag in [
        etag.clone(),
        etag.trim_matches('"').to_string(),
        format!("W/{etag}"),
    ] {
        let unchanged = index(&format!("?since={cursor}"))
            .add_header("if-none-match", tag)
            .await;
        unchanged.assert_status(StatusCode::NOT_MODIFIED);
        assert_eq!(unchanged.headers()["etag"], etag.as_str());
        assert_eq!(unchanged.text(), "");
    }
    // A full download always answers (deal changes reach the phone only
    // so), whatever the ETag.
    for tag in [etag.as_str(), "\"other\""] {
        let answered = index("").add_header("if-none-match", tag).await;
        answered.assert_status_ok();
        assert_eq!(answered.json::<Value>()["full"], true);
    }

    // Bob renamed, Gina deleted, a new customer, a call with Zoë, and a
    // change in company 101.
    sql(
        &pool,
        "UPDATE customers SET name = 'Bob Renamed' WHERE id = 1001",
    )
    .await;
    sql(
        &pool,
        "UPDATE customers SET deleted_at = UTC_TIMESTAMP() WHERE id = 1006",
    )
    .await;
    sql(
        &pool,
        "INSERT INTO customers (id, name, phone, company_id, source, sales_rep)
         VALUES (930201, 'New Lead', '317-555-0201', 100, 'call-in', 100)",
    )
    .await;
    let called = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    add_call(
        &pool,
        980111,
        100,
        Some(100),
        "outbound",
        "answered",
        "+13175550110",
        &called,
        "customer_id = 1010",
    )
    .await;
    sql(
        &pool,
        "UPDATE customers SET name = 'Other Renamed' WHERE id = 2000",
    )
    .await;
    let changed = index(&format!("?since={cursor}"))
        .add_header("if-none-match", etag.clone())
        .await;
    changed.assert_status_ok();
    let new_etag = changed.headers()["etag"].to_str().unwrap().to_string();
    assert_ne!(new_etag, etag);
    let body: Value = changed.json();
    assert_eq!(ids(&body["contacts"]), [1001, 1010, 930_201]);
    assert_eq!(
        (&body["full"], &body["deleted"], &body["nextAfter"]),
        (&json!(false), &json!([1006]), &Value::Null)
    );
    assert_eq!(find(&body["contacts"], 1001)["name"], "Bob Renamed");
    assert_eq!(
        find(&body["contacts"], 1010)["lastCallAt"],
        format!("{}.000Z", called.replace(' ', "T"))
    );
    assert_eq!(body["coworkers"], json!([]));
    assert_ne!(body["cursor"], json!(cursor));
    // A deal that moves changes the ETag too, and the full download (the
    // app's daily one) brings it even when a since-sync already handed out
    // the new ETag.
    sql(&pool, "UPDATE deals SET list_id = 100 WHERE id = 8002").await;
    let synced = index(&format!("?since={cursor}")).await;
    let synced_etag = synced.headers()["etag"].to_str().unwrap().to_string();
    assert_ne!(synced_etag, new_etag);
    let moved = index("").add_header("if-none-match", synced_etag).await;
    moved.assert_status_ok();
    let body: Value = moved.json();
    assert_eq!(body["full"], true);
    // Erin's open deal is in its new stage.
    assert_eq!(
        find(&body["contacts"], 1004)["openDeal"],
        json!({ "id": 8002, "stage": "New" })
    );
    // An unreadable or too old cursor gets a full download.
    for since in ["nonsense", "1600000000.5"] {
        let body: Value = index(&format!("?since={since}")).await.json();
        assert_eq!(body["full"], true);
        assert_eq!(ids(&body["contacts"]).len(), 12);
    }
}

/// Coworkers are every user of the company with a number: direct lines
/// first, then RingCentral, CloudTalk and their own phone, each number
/// once; a user without one, a deleted user and another company's are not.
#[test_db::test(seed)]
async fn coworkers_have_all_their_numbers(pool: MySqlPool) {
    coworker_lines(&pool).await;
    sql(
        &pool,
        "UPDATE users SET ringcentral_phone_number = '(317) 555-0131',
                cloudtalk_phone_number = '+1 317 555 0132', phone_number = '317-555-0131'
          WHERE id = 102",
    )
    .await;
    sql(
        &pool,
        "UPDATE users SET phone_number = '317.555.0133' WHERE id = 104",
    )
    .await;
    sql(
        &pool,
        "UPDATE users SET phone_number = '317-555-0121', cloudtalk_phone_number = 'none'
          WHERE id = 101",
    )
    .await;
    sql(
        &pool,
        "UPDATE users SET phone_number = '317-555-0134' WHERE id IN (105, 106)",
    )
    .await;
    add_session(&pool, 47, 100, 100).await;
    let (server, _) = phone_server(pool.clone()).await;
    let body: Value = server
        .get("/v1/phone/contacts/index")
        .add_header(HEADER, token(47))
        .await
        .json();
    assert_eq!(
        body["coworkers"],
        json!([
            { "userId": 102, "name": "Ada Admin", "e164": "+13175550131", "label": "RingCentral",
              "phones": [
                { "e164": "+13175550131", "label": "RingCentral" },
                { "e164": "+13175550132", "label": "CloudTalk" },
              ] },
            { "userId": 107, "name": "Dina Dealer", "e164": "+13175550122", "label": null,
              "phones": [{ "e164": "+13175550122", "label": null }] },
            { "userId": 101, "name": "Mark Manager", "e164": "+13175550121", "label": "Mark direct",
              "phones": [{ "e164": "+13175550121", "label": "Mark direct" }] },
            { "userId": 104, "name": "Vic Viewer", "e164": "+13175550133", "label": "Mobile",
              "phones": [{ "e164": "+13175550133", "label": "Mobile" }] },
        ])
    );
    // Any of a coworker's numbers names them.
    let matches: Value = server
        .post("/v1/phone/contacts/lookup")
        .add_header(HEADER, token(47))
        .json(&json!({ "numbers": ["3175550132", "+13175550133"] }))
        .await
        .json::<Value>()["matches"]
        .clone();
    assert_eq!(matches[0]["coworker"]["userId"], 102);
    assert_eq!(matches[1]["coworker"]["userId"], 104);
}

#[test_db::test(seed)]
async fn numbers_are_looked_up_in_the_company(pool: MySqlPool) {
    coworker_lines(&pool).await;
    // A newer customer with Alice's phone wins.
    sql(
        &pool,
        "INSERT INTO customers (id, name, phone, company_id, source)
         VALUES (930401, 'Alice Twin', '+1 (317) 555-0100', 100, 'leads')",
    )
    .await;
    add_session(&pool, 45, 100, 100).await;
    let (server, _) = phone_server(pool.clone()).await;
    let lookup = |numbers: Value| {
        server
            .post("/v1/phone/contacts/lookup")
            .add_header(HEADER, token(45))
            .json(&json!({ "numbers": numbers }))
    };
    let response = lookup(json!([
        "+13175550100",
        "317-555-0101",
        "(317) 555-0105",
        "+13175550121",
        "4635550200",
        "555-0100",
        "+13175550123",
    ]))
    .await;
    response.assert_status_ok();
    let matches: Value = response.json::<Value>()["matches"].clone();
    let summary: Vec<(String, Option<i64>, Option<i64>)> = matches
        .as_array()
        .unwrap()
        .iter()
        .map(|found| {
            (
                found["number"].as_str().unwrap().to_string(),
                found["contact"]["id"].as_i64(),
                found["coworker"]["userId"].as_i64(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("+13175550100".to_string(), Some(930_401), None),
            ("317-555-0101".to_string(), Some(1000), None),
            ("(317) 555-0105".to_string(), Some(1004), None),
            ("+13175550121".to_string(), None, Some(101)),
            // Company 101's customer and my own line are nobody here.
            ("4635550200".to_string(), None, None),
            ("555-0100".to_string(), None, None),
            ("+13175550123".to_string(), None, None),
        ]
    );
    assert_eq!(
        matches[1],
        json!({ "number": "317-555-0101", "coworker": null, "contact": {
            "id": 1000, "name": "Alice Lead", "company": null,
            "phones": ["3175550100", "3175550101"], "repId": 100, "mine": true,
            "temperature": "hot", "openDeal": { "id": 8007, "stage": "Archived" },
            "lastCallAt": null } })
    );
    assert_eq!(
        matches[3]["coworker"],
        json!({ "userId": 101, "name": "Mark Manager", "e164": "+13175550121", "label": "Mark direct",
                "phones": [{ "e164": "+13175550121", "label": "Mark direct" }] })
    );
    assert_eq!(
        lookup(json!([])).await.json::<Value>(),
        json!({ "matches": [] })
    );
    // Free-text phones the CRM kept as typed match too (the quick check on
    // the last digits lets every spelling through to the digit compare);
    // one whose last 10 digits differ does not.
    sql(
        &pool,
        "INSERT INTO customers (id, name, phone, phone_2, company_id, source) VALUES
           (930402, 'Spaced', ' 317 555 01 77 ', NULL, 100, 'leads'),
           (930403, 'Dotted', NULL, '1.317.555.0178 (cell)', 100, 'leads'),
           (930404, 'Extension', '317-555-0179 ext 2', NULL, 100, 'leads')",
    )
    .await;
    let odd: Value = lookup(json!(["3175550177", "+13175550178", "3175550179"]))
        .await
        .json();
    let found: Vec<Option<i64>> = odd["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|found| found["contact"]["id"].as_i64())
        .collect();
    assert_eq!(found, [Some(930_402), Some(930_403), None]);
    let too_many: Vec<String> = (0..21).map(|n| format!("31755501{n:02}")).collect();
    lookup(json!(too_many))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

/// Pat, a customer of Mark's with three deals (open, won, lost; one more
/// deleted), notes and two calls.
async fn pat(pool: &MySqlPool) {
    sql(
        pool,
        "INSERT INTO customers (id, name, phone, phone_2, address, city, state, postal_code,
                                company_id, company_name, sales_rep, customer_temperature, source)
         VALUES (930100, 'Pat Card', '317-555-0130', '(317) 555-0131', '9 Card St', 'Indianapolis',
                 'IN', '46201', 100, 'Card Co', 101, 'hot', 'leads')",
    )
    .await;
    sql(
        pool,
        "INSERT INTO customers_emails (id, customer_id, email, created_at) VALUES
           (930110, 930100, 'pat.old@card.test', '2026-01-01 00:00:00'),
           (930111, 930100, 'pat@card.test', '2026-02-01 00:00:00')",
    )
    .await;
    sql(
        pool,
        "UPDATE customers SET email_id = 930111 WHERE id = 930100",
    )
    .await;
    sql(
        pool,
        "INSERT INTO deals (id, customer_id, amount, list_id, position, user_id, is_won, deleted_at) VALUES
           (930120, 930100, 1500.25, 101, 0, 101, NULL, NULL),
           (930121, 930100, 900.00, 101, 1, 101, 1, NULL),
           (930122, 930100, NULL, 100, 2, 101, 0, NULL),
           (930123, 930100, 50.00, 100, 3, 101, NULL, '2026-10-01 00:00:00')",
    )
    .await;
    sql(
        pool,
        "INSERT INTO deal_notes (id, deal_id, company_id, content, created_at, created_by, deleted_at) VALUES
           (930130, 930120, 100, 'First visit', '2026-10-01 10:00:00', 'Rita Rep', NULL),
           (930131, 930120, 100, 'Sent the quote', '2026-10-02 11:00:00', 'Mark Manager', NULL),
           (930132, 930120, 100, 'Deleted note', '2026-10-03 12:00:00', 'Rita Rep', '2026-10-03 13:00:00'),
           (930133, 930122, 100, 'Too pricey', '2026-09-01 09:00:00', NULL, NULL)",
    )
    .await;
    add_call(
        pool,
        980121,
        100,
        Some(100),
        "inbound",
        "answered",
        "+13175550130",
        "2026-10-05 15:00:00",
        "customer_id = 930100, deal_id = 930120, our_number = '+13175550111',
         answered_at = '2026-10-05 15:00:10', ended_at = '2026-10-05 15:04:10', duration_secs = 240,
         recording_status = 'copied', recording_key = 'phone/100/2026/10/980121-call.mp3',
         transcript_status = 'done', ai_note = '- **Project:** kitchen'",
    )
    .await;
    add_call(
        pool,
        980122,
        100,
        Some(101),
        "outbound",
        "no_answer",
        "+13175550131",
        "2026-10-06 16:00:00",
        "customer_id = 930100",
    )
    .await;
}

#[test_db::test(seed)]
async fn a_contact_card_has_deals_notes_and_calls(pool: MySqlPool) {
    pat(&pool).await;
    add_session(&pool, 46, 100, 100).await;
    let (server, _) = phone_server(pool.clone()).await;
    let response = server
        .get("/v1/phone/contacts/930100")
        .add_header(HEADER, token(46))
        .await;
    response.assert_status_ok();
    assert_eq!(
        response.json::<Value>(),
        json!({
            "contact": {
                "id": 930100, "name": "Pat Card", "company": "Card Co",
                "phones": ["3175550130", "3175550131"], "repId": 101, "mine": false,
                "temperature": "hot", "openDeal": { "id": 930120, "stage": "Quoted" },
                "lastCallAt": "2026-10-06T16:00:00.000Z",
                "email": "pat@card.test", "repName": "Mark Manager",
                "address": "9 Card St, Indianapolis, IN 46201",
            },
            "deals": [
                { "id": 930120, "stage": "Quoted", "status": "open", "amount": "1500.25",
                  "lastNote": { "content": "Sent the quote", "createdAt": "2026-10-02T11:00:00.000Z",
                                "createdBy": "Mark Manager" } },
                { "id": 930122, "stage": "New", "status": "lost", "amount": null,
                  "lastNote": { "content": "Too pricey", "createdAt": "2026-09-01T09:00:00.000Z",
                                "createdBy": null } },
                { "id": 930121, "stage": "Quoted", "status": "won", "amount": "900.00", "lastNote": null },
            ],
            "calls": [
                { "id": 980122, "direction": "outbound", "status": "no_answer",
                  "startedAt": "2026-10-06T16:00:00.000Z", "answeredAt": null, "endedAt": null,
                  "durationSecs": null, "otherNumber": "+13175550131",
                  "customer": { "id": 930100, "name": "Pat Card", "company": "Card Co" },
                  "dealId": null, "ourNumber": null, "userId": 101, "userName": "Mark Manager",
                  "hasRecording": false, "transcriptStatus": "none", "aiNote": null, "seen": false,
                  "spamLikely": false, "viaCell": false },
                { "id": 980121, "direction": "inbound", "status": "answered",
                  "startedAt": "2026-10-05T15:00:00.000Z", "answeredAt": "2026-10-05T15:00:10.000Z",
                  "endedAt": "2026-10-05T15:04:10.000Z", "durationSecs": 240,
                  "otherNumber": "+13175550130",
                  "customer": { "id": 930100, "name": "Pat Card", "company": "Card Co" },
                  "dealId": 930120, "ourNumber": { "e164": "+13175550111", "label": "" },
                  "userId": 100, "userName": "Rita Rep", "hasRecording": true,
                  "transcriptStatus": "done", "aiNote": "- **Project:** kitchen", "seen": true,
                  "spamLikely": false, "viaCell": false },
            ],
            "crmUrl": format!("{APP_URL}/employee/customers/info/930100/info"),
        })
    );
    // Another company's customer, a deleted one and the sample: 404.
    for id in [2000, 1003, 990_000_021, 1] {
        server
            .get(&format!("/v1/phone/contacts/{id}"))
            .add_header(HEADER, token(46))
            .await
            .assert_status_not_found();
    }
    // Olga cannot open Pat.
    add_session(&pool, 47, 106, 101).await;
    server
        .get("/v1/phone/contacts/930100")
        .add_header(HEADER, token(47))
        .await
        .assert_status_not_found();
}

#[test_db::test(seed)]
async fn a_new_contact_is_mine_with_an_open_deal(pool: MySqlPool) {
    add_session(&pool, 48, 100, 100).await;
    let (server, _) = phone_server(pool.clone()).await;
    let create = |body: Value| {
        server
            .post("/v1/phone/contacts")
            .add_header(HEADER, token(48))
            .json(&body)
    };
    let response = create(
        json!({ "name": "  New Caller ", "phone": "(317) 555-0188", "company": " Caller Co " }),
    )
    .await;
    response.assert_status(StatusCode::CREATED);
    let body: Value = response.json();
    let id = body["id"].as_i64().unwrap();
    let deal = body["openDeal"]["id"].as_i64().unwrap();
    assert_eq!(
        body,
        json!({ "id": id, "name": "New Caller", "company": "Caller Co", "phones": ["3175550188"],
                "repId": 100, "mine": true, "temperature": null,
                "openDeal": { "id": deal, "stage": "New" }, "lastCallAt": null })
    );
    let row: (
        String,
        String,
        Option<String>,
        String,
        i32,
        bool,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT name, phone, company_name, source, sales_rep, assigned_date IS NOT NULL, created_by
           FROM customers WHERE id = ? AND company_id = 100",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            "New Caller".into(),
            "317-555-0188".into(),
            Some("Caller Co".into()),
            "call-in".into(),
            100,
            true,
            Some("Rita Rep".into())
        )
    );
    let deal_row: (i64, Option<i32>, Option<String>, Option<i32>) = sqlx::query_as(
        "SELECT CAST(customer_id AS SIGNED), user_id, status, is_won FROM deals WHERE id = ?",
    )
    .bind(deal)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(deal_row, (id, Some(100), Some("New".into()), None));
    let history: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT reassigned_by, reassigned_to FROM customers_history WHERE customer_id = ?",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(history, (Some("Rita Rep".into()), Some("Rita Rep".into())));
    // No company is fine.
    let plain: Value = create(json!({ "name": "Solo", "phone": "+13175550189", "company": null }))
        .await
        .json();
    assert_eq!(plain["company"], Value::Null);
    for (body, error) in [
        (
            json!({ "name": "X", "phone": "+44 20 7946 0958" }),
            "invalid_phone",
        ),
        (json!({ "name": "X", "phone": "555-0188" }), "invalid_phone"),
        // +1 numbers outside the US and Canada, or premium and special ones.
        (
            json!({ "name": "X", "phone": "+1 876 555 0188" }),
            "invalid_phone",
        ),
        (
            json!({ "name": "X", "phone": "900-555-0188" }),
            "invalid_phone",
        ),
        (
            json!({ "name": "X", "phone": "317-976-0188" }),
            "invalid_phone",
        ),
        (
            json!({ "name": "  ", "phone": "3175550188" }),
            "Enter a name of 1 to 255 characters.",
        ),
        (
            json!({ "name": "x".repeat(256), "phone": "3175550188" }),
            "Enter a name of 1 to 255 characters.",
        ),
    ] {
        let response = create(body).await;
        response.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(response.json::<Value>(), json!({ "error": error }));
    }
    // Nothing of a refused contact was kept: no customer, deal or audit line.
    let made: i64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS SIGNED) FROM customers WHERE company_id = 100 AND name = 'X'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(made, 0);
}

#[test_db::test(seed)]
async fn numbers_fill_phone_then_phone_2_and_ask_before_replacing(pool: MySqlPool) {
    sql(
        &pool,
        "INSERT INTO customers (id, name, phone, phone_2, company_id, source)
         VALUES (930300, 'Phone Slots', '  ', NULL, 100, 'leads')",
    )
    .await;
    add_session(&pool, 49, 100, 100).await;
    let (server, _) = phone_server(pool.clone()).await;
    let add = |id: i64, body: Value| {
        server
            .post(&format!("/v1/phone/contacts/{id}/phones"))
            .add_header(HEADER, token(49))
            .json(&body)
    };
    let phones = || async {
        sqlx::query_as::<_, (Option<String>, Option<String>)>(
            "SELECT phone, phone_2 FROM customers WHERE id = 930300",
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let first = add(930_300, json!({ "phone": "3175550301" })).await;
    first.assert_status_ok();
    assert_eq!(first.json::<Value>()["phones"], json!(["3175550301"]));
    assert_eq!(phones().await, (Some("317-555-0301".into()), None));
    add(
        930_300,
        json!({ "phone": "+1 317 555 0302", "replacePhone2": false }),
    )
    .await
    .assert_status_ok();
    assert_eq!(
        phones().await,
        (Some("317-555-0301".into()), Some("317-555-0302".into()))
    );
    // A number the customer has changes nothing.
    let same = add(930_300, json!({ "phone": "(317) 555-0302" })).await;
    same.assert_status_ok();
    assert_eq!(
        same.json::<Value>()["phones"],
        json!(["3175550301", "3175550302"])
    );
    // Both used: ask first.
    let taken = add(930_300, json!({ "phone": "3175550303" })).await;
    taken.assert_status(StatusCode::CONFLICT);
    assert_eq!(
        taken.json::<Value>(),
        json!({ "error": "phone_2_taken", "phone2": "3175550302" })
    );
    let replaced = add(
        930_300,
        json!({ "phone": "3175550303", "replacePhone2": true }),
    )
    .await;
    replaced.assert_status_ok();
    assert_eq!(
        replaced.json::<Value>()["phones"],
        json!(["3175550301", "3175550303"])
    );
    assert_eq!(
        phones().await,
        (Some("317-555-0301".into()), Some("317-555-0303".into()))
    );
    for bad in ["12-34", "+1 809 555 0304", "+1 500 555 0304"] {
        add(930_300, json!({ "phone": bad }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    for id in [2000, 1003, 990_000_021] {
        add(id, json!({ "phone": "3175550304" }))
            .await
            .assert_status_not_found();
    }
}

#[test_db::test(seed)]
async fn new_contacts_and_added_numbers_are_matched_at_once(pool: MySqlPool) {
    // Company 100's phone index counts as filled but holds nothing yet, so
    // the lookups below read the index alone.
    sql(
        &pool,
        "INSERT INTO customer_phone_index_builds (company_id, built_at) VALUES (100, UTC_TIMESTAMP())",
    )
    .await;
    add_session(&pool, 51, 100, 100).await;
    let (server, _) = phone_server(pool.clone()).await;
    let matched = |numbers: Value| {
        let request = server
            .post("/v1/phone/contacts/lookup")
            .add_header(HEADER, token(51))
            .json(&json!({ "numbers": numbers }));
        async move {
            let body: Value = request.await.json();
            body["matches"]
                .as_array()
                .unwrap()
                .iter()
                .map(|found| found["contact"]["id"].as_i64())
                .collect::<Vec<_>>()
        }
    };
    // Alice is not in the (empty) index.
    assert_eq!(matched(json!(["3175550100"])).await, [None]);
    // A contact made in the app is matched at once.
    let created: Value = server
        .post("/v1/phone/contacts")
        .add_header(HEADER, token(51))
        .json(&json!({ "name": "Fresh Caller", "phone": "(317) 555-0191" }))
        .await
        .json();
    let id = created["id"].as_i64().unwrap();
    assert_eq!(matched(json!(["+13175550191"])).await, [Some(id)]);
    // A number added to Bob: both his numbers are matched now, and a phone 2
    // replaced no longer is.
    let add = |body: Value| {
        server
            .post("/v1/phone/contacts/1001/phones")
            .add_header(HEADER, token(51))
            .json(&body)
    };
    add(json!({ "phone": "3175550192" }))
        .await
        .assert_status_ok();
    assert_eq!(
        matched(json!(["3175550102", "3175550192"])).await,
        [Some(1001), Some(1001)]
    );
    add(json!({ "phone": "3175550193", "replacePhone2": true }))
        .await
        .assert_status_ok();
    assert_eq!(
        matched(json!(["3175550192", "3175550193"])).await,
        [None, Some(1001)]
    );
    let rows: Vec<(i32, String, i64)> = sqlx::query_as(
        "SELECT company_id, last10, CAST(customer_id AS SIGNED) FROM customer_phone_index
          WHERE customer_id IN (1001, ?) ORDER BY customer_id, last10",
    )
    .bind(id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            (100, "3175550102".to_string(), 1001),
            (100, "3175550193".to_string(), 1001),
            (100, "3175550191".to_string(), id),
        ]
    );
    // A row naming another company's customer is never matched.
    sql(
        &pool,
        "INSERT INTO customer_phone_index (company_id, last10, customer_id) VALUES (100, '4635550200', 2000)",
    )
    .await;
    assert_eq!(matched(json!(["4635550200"])).await, [None]);
}

#[test_db::test(seed)]
async fn contact_routes_take_only_a_live_device_token(pool: MySqlPool) {
    add_phone(&pool, 50, 100, 100).await;
    revoke_session(&pool, 50).await;
    let (server, _) = phone_server(pool.clone()).await;
    assert_device_only(
        &server,
        50,
        &[
            (Method::GET, "/v1/phone/contacts/index".to_string()),
            (Method::POST, "/v1/phone/contacts/lookup".to_string()),
            (Method::GET, "/v1/phone/contacts/1000".to_string()),
            (Method::POST, "/v1/phone/contacts".to_string()),
            (Method::POST, "/v1/phone/contacts/1000/phones".to_string()),
        ],
    )
    .await;
}
