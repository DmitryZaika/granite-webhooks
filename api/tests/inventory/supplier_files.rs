use crate::common;

use api::ai::Ai;
use api::app;
use api::price_lists::build_context;
use api::state::AppState;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

static NEXT_ID: AtomicI32 = AtomicI32::new(0);

fn next_id() -> i32 {
    9500 + NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

async fn supplier(pool: &MySqlPool, company_id: i32, name: &str) -> i32 {
    let id = next_id();
    sqlx::query("INSERT INTO suppliers (id, supplier_name, company_id) VALUES (?, ?, ?)")
        .bind(id)
        .bind(name)
        .bind(company_id)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn file(pool: &MySqlPool, supplier_id: i32, name: &str, status: &str) -> i32 {
    let id = next_id();
    sqlx::query(
        "INSERT INTO supplier_files (id, name, url, supplier_id, status, error_token)
         VALUES (?, ?, ?, ?, ?, IF(? = 'failed', 'extract_error', NULL))",
    )
    .bind(id)
    .bind(name)
    .bind(format!("https://files.example/{id}.pdf"))
    .bind(supplier_id)
    .bind(status)
    .bind(status)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn chunk(pool: &MySqlPool, file_id: i32, index: i32, heading: &str, content: &str) {
    sqlx::query(
        "INSERT INTO supplier_file_chunks (supplier_file_id, chunk_index, heading, content) VALUES (?, ?, ?, ?)",
    )
    .bind(file_id)
    .bind(index)
    .bind(heading)
    .bind(content)
    .execute(pool)
    .await
    .unwrap();
}

fn names(sources: &[api::price_lists::PriceListSource]) -> Vec<String> {
    let mut names: Vec<String> = sources.iter().map(|s| s.name.clone()).collect();
    names.sort();
    names
}

// ---- The context the model reads (port of supplierChatContext.test.ts)

#[test_db::test(seed)]
async fn the_chunk_with_the_color_is_selected_and_its_file_cited(pool: MySqlPool) {
    let msi = supplier(&pool, 100, "MSI").await;
    let list = file(&pool, msi, "MSI Price List", "ready").await;
    chunk(
        &pool,
        list,
        0,
        "Group 1",
        "| Alaska White | 3cm | 1 | 130x65 | $21.00 |",
    )
    .await;
    chunk(
        &pool,
        list,
        1,
        "Group 2",
        "| Black Pearl | 3cm | 2 | 126x63 | $14.50 |",
    )
    .await;
    let found = build_context(&pool, 100, "Price for Alaska White from MSI")
        .await
        .unwrap();
    assert!(found.context.contains("Alaska White"));
    assert_eq!(found.sources[0].name, "MSI Price List");
    assert_eq!(found.sources[0].id, i64::from(list));
    assert_eq!(found.sources[0].supplier_name, "MSI");
    assert!(
        found
            .context
            .contains(&format!("SOURCE {list}: MSI — MSI Price List"))
    );
    assert!(found.context.contains("### Group 1\n| Alaska White"));
    assert!(found.context.contains("MUST check every SOURCE"));
    assert!(found.context.contains("not only the first"));
    assert_eq!(found.supplier_name, "MSI");
}

#[test_db::test(seed)]
async fn every_ready_list_is_included_so_a_later_file_is_found(pool: MySqlPool) {
    let msi = supplier(&pool, 100, "MSI").await;
    let first = file(&pool, msi, "MSI Price List Part 1", "ready").await;
    chunk(
        &pool,
        first,
        0,
        "Group 1",
        &format!(
            "| Alaska White | 3cm | 1 | 130x65 | $21.00 | {}",
            "y".repeat(8500)
        ),
    )
    .await;
    let second = file(&pool, msi, "MSI Price List Part 2", "ready").await;
    chunk(
        &pool,
        second,
        0,
        "Group 4",
        "| Calacatta Edelia | 3cm | 4 | 126x63 | $48.50 |",
    )
    .await;
    let found = build_context(&pool, 100, "Price for Kolokata Edelia from MSI")
        .await
        .unwrap();
    assert_eq!(
        names(&found.sources),
        ["MSI Price List Part 1", "MSI Price List Part 2"]
    );
    assert!(found.context.contains("Calacatta Edelia"));
    assert!(found.context.contains("$48.50"));
}

#[test_db::test(seed)]
async fn all_seven_lists_are_represented(pool: MySqlPool) {
    let msi = supplier(&pool, 100, "MSI").await;
    let mut expected = Vec::new();
    for index in 1..=7 {
        let name = format!("MSI Volume {index}");
        let id = file(&pool, msi, &name, "ready").await;
        let content = if index == 7 {
            "| Calacatta Edelia | 3cm | 4 | 126x63 | $48.50 |".to_string()
        } else {
            format!(
                "| Filler Color {index} | 3cm | {index} | 130x65 | $1{index}.00 | {}",
                "z".repeat(1800)
            )
        };
        chunk(&pool, id, 0, &format!("Group {index}"), &content).await;
        expected.push(name);
    }
    let found = build_context(&pool, 100, "Price for Kolokata Edelia from MSI")
        .await
        .unwrap();
    assert_eq!(names(&found.sources), expected);
    for name in &expected {
        assert!(found.context.contains(&format!("MSI — {name}")), "{name}");
    }
    assert!(found.context.contains("$48.50"));
}

#[test_db::test(seed)]
async fn the_group_file_and_the_group_price_file_come_together(pool: MySqlPool) {
    let msi = supplier(&pool, 100, "MSI").await;
    let groups = file(&pool, msi, "MSI Color Groups", "ready").await;
    chunk(
        &pool,
        groups,
        0,
        "Group 4",
        "| Calacatta Edelia | 3cm | 4 | 126x63 | - |",
    )
    .await;
    let prices = file(&pool, msi, "MSI Group Prices", "ready").await;
    chunk(
        &pool,
        prices,
        0,
        "Group prices",
        "GROUP 4 SLAB PRICE $48.50",
    )
    .await;
    let found = build_context(&pool, 100, "Price for Calacatta Edelia from MSI")
        .await
        .unwrap();
    assert_eq!(
        names(&found.sources),
        ["MSI Color Groups", "MSI Group Prices"]
    );
    assert!(found.context.contains("GROUP 4 SLAB PRICE $48.50"));
}

#[test_db::test(seed)]
async fn only_ready_files_are_searched_and_the_rest_is_noted(pool: MySqlPool) {
    let msi = supplier(&pool, 100, "MSI").await;
    let a = file(&pool, msi, "MSI Ready A", "ready").await;
    chunk(
        &pool,
        a,
        0,
        "Group 1",
        "| Alpha Stone | 3cm | 1 | 130x65 | $10.00 |",
    )
    .await;
    let b = file(&pool, msi, "MSI Ready B", "ready").await;
    chunk(
        &pool,
        b,
        0,
        "Group 2",
        "| Beta Stone | 3cm | 2 | 126x63 | $20.00 |",
    )
    .await;
    let processing = file(&pool, msi, "MSI Processing", "processing").await;
    chunk(&pool, processing, 0, "Old", "| Stale Stone | $1 |").await;
    file(&pool, msi, "MSI Failed", "failed").await;
    let found = build_context(&pool, 100, "Price for Beta from MSI")
        .await
        .unwrap();
    assert_eq!(names(&found.sources), ["MSI Ready A", "MSI Ready B"]);
    assert!(!found.context.contains("MSI Processing"));
    assert!(!found.context.contains("Stale Stone"));
    assert!(found.context.contains("$20.00"));
    assert!(found.context.ends_with(
        "\nNOTE: of 4 price files for MSI, 1 are still being processed and 1 could not be processed; the answer may be incomplete."
    ), "{}", found.context);
    let readiness = found.readiness.unwrap();
    assert_eq!(
        (
            readiness.total,
            readiness.ready,
            readiness.processing,
            readiness.failed
        ),
        (4, 2, 1, 1)
    );
}

#[test_db::test(seed)]
async fn a_section_slab_size_fills_a_later_chunk(pool: MySqlPool) {
    let msi = supplier(&pool, 100, "MSI Surfaces").await;
    let list = file(&pool, msi, "MSI Price List 9/15/2026", "ready").await;
    chunk(
        &pool,
        list,
        0,
        "Q+ Premium 2cm",
        "| color/product | thickness | group/level | slab size | price |\n|---|---|---|---|---|\n| Calacatta Anava 2cm BM | 2 cm | Q+ | 127x64 | $29.82 |",
    )
    .await;
    chunk(
        &pool,
        list,
        1,
        "Q+ Premium 2cm",
        "| color/product | thickness | group/level | slab size | price |\n|---|---|---|---|---|\n| Calacatta Lapiza 2cm BM | 2 cm | Q+ | - | $29.82 |",
    )
    .await;
    let found = build_context(&pool, 100, "Price for calacatta lapiza from MSI Surfaces")
        .await
        .unwrap();
    assert!(
        found
            .context
            .contains("| Calacatta Lapiza 2cm BM | 2 cm | Q+ | 127x64 | $29.82 |")
    );
    assert!(found.context.contains("one slab size for the block"));
}

#[test_db::test(seed)]
async fn the_budget_holds_and_every_huge_list_is_represented(pool: MySqlPool) {
    let msi = supplier(&pool, 100, "MSI").await;
    for index in 1..=3 {
        let id = file(&pool, msi, &format!("MSI Huge {index}"), "ready").await;
        chunk(
            &pool,
            id,
            0,
            &format!("Group {index}"),
            &format!(
                "| Color {index} | 3cm | {index} | 130x65 | $3{index}.00 | {}",
                "w".repeat(5000)
            ),
        )
        .await;
    }
    let found = build_context(&pool, 100, "Price for Color 3 from MSI")
        .await
        .unwrap();
    assert_eq!(
        names(&found.sources),
        ["MSI Huge 1", "MSI Huge 2", "MSI Huge 3"]
    );
    assert!(found.context.contains("Color 3"));
    assert!(found.context.chars().count() <= 12_000);

    let cambria = supplier(&pool, 100, "Cambria").await;
    let list = file(&pool, cambria, "Cambria Price List", "ready").await;
    for index in 0..10 {
        chunk(
            &pool,
            list,
            index,
            &format!("Group {index}"),
            &format!("Brittanicca ${index}.00 {}", "x".repeat(1980)),
        )
        .await;
    }
    let found = build_context(&pool, 100, "Price for Brittanicca from Cambria")
        .await
        .unwrap();
    assert_eq!(names(&found.sources), ["Cambria Price List"]);
    assert!(found.context.chars().count() <= 12_000);
}

#[test_db::test(seed)]
async fn another_companys_lists_never_show(pool: MySqlPool) {
    let mine = supplier(&pool, 100, "Shared Stone").await;
    let a = file(&pool, mine, "Company A Price List", "ready").await;
    chunk(
        &pool,
        a,
        0,
        "Group 1",
        "| Company A Exclusive Color | 3cm | 1 | 130x65 | $21.00 |",
    )
    .await;
    let theirs = supplier(&pool, 101, "Shared Stone").await;
    let b = file(&pool, theirs, "Company B Price List", "ready").await;
    chunk(
        &pool,
        b,
        0,
        "Group 1",
        "| Company B Exclusive Color | 3cm | 1 | 130x65 | $99.00 |",
    )
    .await;
    let found = build_context(
        &pool,
        100,
        "Price for Company A Exclusive Color from Shared Stone",
    )
    .await
    .unwrap();
    assert!(found.context.contains("Company A Exclusive Color"));
    assert!(!found.context.contains("Company B Exclusive Color"));
    assert_eq!(names(&found.sources), ["Company A Price List"]);
}

#[test_db::test(seed)]
async fn readiness_is_reported_with_or_without_a_named_supplier(pool: MySqlPool) {
    sqlx::query("DELETE FROM suppliers WHERE company_id = 101")
        .execute(&pool)
        .await
        .ok();
    let cosentino = supplier(&pool, 101, "Cosentino").await;
    file(&pool, cosentino, "Dekton list", "processing").await;
    let named = build_context(&pool, 101, "Price for Dekton from Cosentino")
        .await
        .unwrap();
    assert!(named.sources.is_empty());
    assert_eq!(named.supplier_name, "Cosentino");
    assert_eq!(named.readiness.unwrap().processing, 1);
    assert!(
        named
            .context
            .contains("No matching supplier price-list content was found.")
    );
    let unnamed = build_context(&pool, 101, "Price for Alaska White 3cm")
        .await
        .unwrap();
    assert!(unnamed.sources.is_empty());
    assert_eq!(unnamed.supplier_name, "");
    assert_eq!(unnamed.readiness.unwrap().ready, 0);
    assert!(unnamed.readiness.unwrap().processing >= 1);
    assert!(!unnamed.context.contains("NOTE:"));
}

// ---- GET /v1/suppliers/file-readiness

#[test_db::test(seed)]
async fn file_readiness_counts_the_named_suppliers_files(pool: MySqlPool) {
    let msi = supplier(&pool, 100, " MSI ").await;
    file(&pool, msi, "a", "ready").await;
    file(&pool, msi, "b", "pending").await;
    file(&pool, msi, "c", "processing").await;
    file(&pool, msi, "d", "failed").await;
    let other = supplier(&pool, 101, "MSI").await;
    file(&pool, other, "e", "ready").await;
    let server = server(pool);
    let body = server
        .get("/v1/suppliers/file-readiness?name=%20MSI")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(
        body,
        json!({ "readiness": { "total": 4, "ready": 1, "processing": 2, "failed": 1 } })
    );
    let no_files = server
        .get("/v1/suppliers/file-readiness?name=Stone%20Source%20Inc")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(
        no_files,
        json!({ "readiness": { "total": 0, "ready": 0, "processing": 0, "failed": 0 } })
    );
    for path in [
        "/v1/suppliers/file-readiness?name=Nobody",
        "/v1/suppliers/file-readiness",
    ] {
        let body = server
            .get(path)
            .with_session(sessions::REP)
            .await
            .json::<Value>();
        assert_eq!(body, json!({ "readiness": null }), "{path}");
    }
    server
        .get("/v1/suppliers/file-readiness?name=MSI")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

// ---- POST /v1/supplier-files/{id}/retry

#[test_db::test(seed)]
async fn an_admin_requeues_only_a_failed_file_of_the_company(pool: MySqlPool) {
    let msi = supplier(&pool, 100, "MSI").await;
    let failed = file(&pool, msi, "a", "failed").await;
    let ready = file(&pool, msi, "b", "ready").await;
    sqlx::query("UPDATE supplier_files SET attempts = 3 WHERE id = ?")
        .bind(failed)
        .execute(&pool)
        .await
        .unwrap();
    let other = supplier(&pool, 101, "Theirs").await;
    let theirs = file(&pool, other, "c", "failed").await;
    let server = server(pool.clone());

    server
        .post(&format!("/v1/supplier-files/{failed}/retry"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .post(&format!("/v1/supplier-files/{failed}/retry"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let row: (String, i32, Option<String>, bool) = sqlx::query_as(
        "SELECT status, attempts, error_token, status_updated_at IS NOT NULL FROM supplier_files WHERE id = ?",
    )
    .bind(failed)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row, ("pending".into(), 0, None, true));
    for id in [failed, ready, theirs, 999_999] {
        server
            .post(&format!("/v1/supplier-files/{id}/retry"))
            .with_session(sessions::ADMIN)
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }
    let theirs_status: String =
        sqlx::query_scalar("SELECT status FROM supplier_files WHERE id = ?")
            .bind(theirs)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(theirs_status, "failed");
}

// ---- GET /v1/suppliers/price-offers

async fn ai_server(pool: &MySqlPool, reply: &'static str) -> (TestServer, Arc<Mutex<Vec<Value>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::clone(&seen);
    let router = Router::new().route(
        "/chat",
        post(move |Json(body): Json<Value>| {
            let calls = Arc::clone(&calls);
            async move {
                calls.lock().unwrap().push(body);
                Json(json!({
                    "choices": [{ "message": { "content": reply } }],
                    "usage": { "prompt_tokens": 10, "completion_tokens": 5 },
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_ai(Ai::new(Some("key"), &format!("http://{address}/chat")));
    (TestServer::new(app(state)), seen)
}

#[test_db::test(seed)]
async fn offers_come_from_the_price_lists_through_the_ai(pool: MySqlPool) {
    sqlx::query("UPDATE company SET state_taxes = 0.0725 WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();
    let msi = supplier(&pool, 100, "MSI").await;
    let list = file(&pool, msi, "MSI 2026.pdf", "ready").await;
    chunk(
        &pool,
        list,
        0,
        "Group 1",
        "| Calacatta Laza | 3cm | 1 | 126x63 | $28.50 |",
    )
    .await;
    file(&pool, msi, "MSI new.pdf", "pending").await;
    let (server, seen) = ai_server(
        &pool,
        r#"{"offers":[{"supplier":"MSI","name":"Calacatta Laza","thickness":"3cm","price":1500,"unit":"sqft","slabLength":63,"slabWidth":126,"file":"MSI 2026.pdf"},{"supplier":"MSI","name":"Laza","price":28.5,"unit":"slab","slabLength":null,"slabWidth":null,"file":"MSI 2026.pdf"}]}"#,
    )
    .await;
    let body = server
        .get("/v1/suppliers/price-offers?q=Price%20for%20Calacatta%20Laza%20from%20MSI")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    let url = format!("https://files.example/{list}.pdf");
    assert_eq!(
        body,
        json!({
            "offers": [
                { "supplier": "MSI", "name": "Calacatta Laza", "thickness": "3cm", "price": 1500.0,
                  "unit": "slab", "slab": [126.0, 63.0], "file": "MSI 2026.pdf",
                  "file_id": list, "url": url },
                // The list says per slab: no size to say otherwise.
                { "supplier": "MSI", "name": "Laza", "thickness": "", "price": 28.5,
                  "unit": "slab", "slab": null, "file": "MSI 2026.pdf",
                  "file_id": list, "url": url },
            ],
            "incomplete": true,
            "tax_pct": 7.25,
        })
    );
    let sent = seen.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["response_format"], json!({ "type": "json_object" }));
    let user = sent[0]["messages"][1]["content"].as_str().unwrap();
    assert!(user.starts_with("Request: Price for Calacatta Laza from MSI\n\nSUPPLIER PRICE LISTS"));
    assert!(user.contains("| Calacatta Laza | 3cm | 1 | 126x63 | $28.50 |"));
    let feature: String =
        sqlx::query_scalar("SELECT feature FROM ai_usage_log ORDER BY id DESC LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(feature, "chat");
}

#[test_db::test(seed)]
async fn no_ai_call_without_a_query_or_matching_files(pool: MySqlPool) {
    let (server, seen) = ai_server(&pool, "{}").await;
    let short = server
        .get("/v1/suppliers/price-offers?q=a")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(
        short,
        json!({ "offers": [], "incomplete": false, "tax_pct": 0.0 })
    );
    // Seeded suppliers have no processed files.
    let nothing = server
        .get("/v1/suppliers/price-offers?q=Price%20for%20Laza")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(nothing["offers"], json!([]));
    assert!(seen.lock().unwrap().is_empty());
    server
        .get("/v1/suppliers/price-offers?q=Laza")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

#[test_db::test(seed)]
async fn a_color_search_reads_only_the_picked_suppliers_files(pool: MySqlPool) {
    let msi = supplier(&pool, 100, "MSI").await;
    let list = file(&pool, msi, "MSI 2026.pdf", "ready").await;
    chunk(
        &pool,
        list,
        0,
        "Q Premium",
        "| Calacatta Laza | 3cm | 126x63 | $28.50 |",
    )
    .await;
    // Close names a fuzzy match would also take.
    let surfaces = supplier(&pool, 100, "MSI Surfaces").await;
    let other = file(&pool, surfaces, "Surfaces.pdf", "ready").await;
    chunk(&pool, other, 0, "", "| Calacatta Laza | 3cm | $31.00 |").await;
    let cosentino = supplier(&pool, 100, "Cosentino").await;
    let theirs = file(&pool, cosentino, "Cosentino.pdf", "ready").await;
    chunk(
        &pool,
        theirs,
        0,
        "",
        "| Calacatta Laza Gold | 2cm | $40.00 |",
    )
    .await;
    let (server, seen) = ai_server(
        &pool,
        r#"{"offers":[{"supplier":"MSI","name":"Calacatta Laza","thickness":"3cm","price":28.5,"unit":"sqft","slabLength":126,"slabWidth":63,"file":"MSI 2026.pdf"}]}"#,
    )
    .await;
    let get = |query: &'static str| {
        server
            .get(&format!("/v1/suppliers/price-offers?{query}"))
            .with_session(sessions::REP)
    };

    // The exact name (trimmed, any case), not every name like it.
    let body = get("supplier=%20msi%20&color=Calacatta%20Laza&q=ignored")
        .await
        .json::<Value>();
    assert_eq!(body["offers"][0]["file_id"], json!(list));
    let request = |i: usize| {
        seen.lock().unwrap()[i]["messages"][1]["content"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let sent = request(0);
    assert!(sent.starts_with("Request: Price for Calacatta Laza from msi\n\nSUPPLIER PRICE LISTS"));
    assert!(sent.contains("| Calacatta Laza | 3cm | 126x63 | $28.50 |"));
    assert!(!sent.contains("$31.00"));
    assert!(!sent.contains("$40.00"));

    // No supplier by that name: the closest one, and still only that one.
    get("supplier=Cosentin&color=Calacatta%20Laza").await;
    let sent = request(1);
    assert!(sent.contains("$40.00"));
    assert!(!sent.contains("$28.50"));

    // No supplier is close: nothing is read and the AI is not asked.
    let none = get("supplier=Nobody&color=Calacatta%20Laza")
        .await
        .json::<Value>();
    assert_eq!(none["offers"], json!([]));
    assert_eq!(none["incomplete"], json!(false));
    let short = get("supplier=MSI&color=C").await.json::<Value>();
    assert_eq!(short["offers"], json!([]));
    assert_eq!(seen.lock().unwrap().len(), 2);
}

#[test_db::test(seed)]
async fn a_color_search_puts_the_named_color_first(pool: MySqlPool) {
    let cosentino = supplier(&pool, 100, "Cosentino").await;
    let list = file(&pool, cosentino, "Silestone 2026", "ready").await;
    // Too big to both fit. The first has both words, the second the name.
    for (index, rows) in [
        "| Stellar Snow | 2cm | $2,042.23 |\n| Night Sky | 2cm | $1,990.00 |",
        "| Stellar Night | 2cm | $2,609.33 |",
    ]
    .into_iter()
    .enumerate()
    {
        chunk(
            &pool,
            list,
            i32::try_from(index).unwrap(),
            "Silestone",
            &format!("{rows}\n{}", "s".repeat(5000)),
        )
        .await;
    }
    let found = api::price_lists::build_color_context(&pool, 100, "Cosentino", "stellar night")
        .await
        .unwrap();
    assert_eq!(names(&found.sources), ["Silestone 2026"]);
    assert!(found.context.contains("| Stellar Night |"));
    assert!(!found.context.contains("| Night Sky |"));
    assert_eq!(found.supplier_name, "Cosentino");
}

#[test_db::test(seed)]
async fn a_bad_ai_reply_is_an_empty_list(pool: MySqlPool) {
    let msi = supplier(&pool, 100, "MSI").await;
    let list = file(&pool, msi, "MSI.pdf", "ready").await;
    chunk(&pool, list, 0, "", "| Laza | $28 |").await;
    let (server, _) = ai_server(&pool, r#"{"offers":[{"name":"Laza","price":-1}]}"#).await;
    let body = server
        .get("/v1/suppliers/price-offers?q=Laza%20MSI")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(body["offers"], json!([]));
    assert_eq!(body["incomplete"], json!(false));
}
