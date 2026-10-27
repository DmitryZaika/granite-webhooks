//! Uploaded files belong to the company that uploaded them. Keys are
//! `<folder>/<company id>/<uuid>`; bucket URLs are public (the showroom shows
//! them), so every route that takes a file URL must refuse another company's
//! file, and every route that deletes or overwrites a file must leave other
//! companies' files (and older keys that name no company) alone.

use crate::common;

use api::auth::cookie::SessionData;
use api::routes::emails::attachment_token;
use api::routes::uploads::schemas::UploadFolder;
use api::routes::uploads::{accepted_key, company_key, owned_by};
use api::storage::{MEMORY_PUBLIC_BASE, Storage, icon_key};
use axum::http::StatusCode;
use axum_test::{TestRequest, TestResponse, TestServer};
use common::{SECRET, WithSession, cookie_for, png, server_with_storage, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Seed: stone 3000 (company 100) with installed photos, stone 3008 for
// slabs, sink 3100 and faucet 3200 of company 100, supplier 301 of company
// 100, deal 8601 of Dina (DEALER), email template 8670 the admin may edit.
// Event 8950 of company 100 comes from `EVENT`.
const EVENT: &str = "
INSERT INTO calendars (id, company_id, name, color) VALUES (8803, 100, 'Crew', 'teal');
INSERT INTO events (id, title, location, start_date, end_date, created_user_id, assigned_user_id, calendar_id) VALUES
  (8950, '1000', '9 Elm St', '2026-11-02 15:00:00', '2026-11-02 16:00:00', 101, 100, 8803);
";

fn url(key: &str) -> String {
    format!("{MEMORY_PUBLIC_BASE}/{key}")
}

/// Stores a PNG (and its thumbnail) at `key`, as a finished upload would.
async fn stored(storage: &Storage, key: &str) {
    storage.put(key, png(40, 30), "image/png").await.unwrap();
    storage
        .put(&icon_key(key), png(4, 3), "image/png")
        .await
        .unwrap();
}

/// Whether `key` and its thumbnail are still in the bucket, unchanged.
async fn intact(storage: &Storage, key: &str) -> bool {
    let same = |object: Option<api::storage::StoredObject>, bytes: Vec<u8>| {
        object.is_some_and(|object| object.bytes == bytes)
    };
    same(storage.get(key).await.unwrap(), png(40, 30))
        && same(storage.get(&icon_key(key)).await.unwrap(), png(4, 3))
}

async fn gone(storage: &Storage, key: &str) -> bool {
    storage.get(key).await.unwrap().is_none()
        && storage.get(&icon_key(key)).await.unwrap().is_none()
}

/// A fresh upload of the caller's company through `POST /v1/uploads`.
async fn upload(server: &TestServer, storage: &Storage, session: &str, folder: &str) -> String {
    let res = server
        .post("/v1/uploads")
        .with_session(session)
        .json(&json!({ "folder": folder, "file_name": "a.png", "content_type": "image/png" }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let url = res.json::<Value>()["url"].as_str().unwrap().to_string();
    let key = storage.key_for_url(&url).unwrap();
    storage.put(&key, png(40, 30), "image/png").await.unwrap();
    url
}

async fn count(pool: &MySqlPool, sql: &str, url: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
        .bind(url)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn stone_body(image_url: &str, n: usize) -> Value {
    json!({
        "name": format!("Blue Pearl {n}"), "type": "Granite", "finishing": "polished",
        "image_url": image_url, "is_display": true, "on_sale": false, "regular_stock": true,
        "supplier_id": 301, "length": 120.5, "width": 70, "cost_per_sqft": 45,
        "retail_price": 99, "level": 3, "bundle_number": "BP-1", "color_ids": [3]
    })
}

fn sink_body(image_url: &str, n: usize) -> Value {
    json!({
        "name": format!("Undermount {n}"), "type": "stainless 18 gauge", "image_url": image_url,
        "is_display": true, "regular_stock": false, "supplier_id": 300, "length": 32,
        "width": 19, "retail_price": 199, "cost": 90
    })
}

fn faucet_body(image_url: &str, n: usize) -> Value {
    json!({ "name": format!("Touchless {n}"), "type": "single handle", "image_url": image_url,
            "is_display": true, "retail_price": 249 })
}

/// A route that takes an uploaded file's URL.
struct Taker {
    name: &'static str,
    folder: &'static str,
    session: &'static str,
    request: fn(&TestServer, &str, usize) -> TestRequest,
    /// Counts the records holding the URL (`?`); `None` when it keeps none.
    stored: Option<&'static str>,
    /// Takes an older upload whose key names no company. Routes that delete
    /// or overwrite the file they are given never do.
    takes_legacy: bool,
}

const TAKERS: &[Taker] = &[
    Taker {
        name: "create stone",
        folder: "stones",
        session: sessions::ADMIN,
        request: |s, u, n| s.post("/v1/stones").json(&stone_body(u, n)),
        stored: Some("SELECT COUNT(*) FROM stones WHERE url = ?"),
        takes_legacy: true,
    },
    Taker {
        name: "update stone",
        folder: "stones",
        session: sessions::ADMIN,
        request: |s, u, n| s.put("/v1/stones/3000").json(&stone_body(u, n)),
        stored: Some("SELECT COUNT(*) FROM stones WHERE url = ?"),
        takes_legacy: true,
    },
    Taker {
        name: "stone photos",
        folder: "stones",
        session: sessions::ADMIN,
        request: |s, u, _| {
            s.post("/v1/stones/3000/images")
                .json(&json!({ "urls": [u] }))
        },
        stored: Some("SELECT COUNT(*) FROM installed_stones WHERE url = ?"),
        takes_legacy: true,
    },
    Taker {
        name: "slab photo",
        folder: "stones",
        session: sessions::ADMIN,
        request: |s, u, n| {
            s.post("/v1/stones/3008/slabs")
                .json(&json!({ "bundle": format!("Z-{n}"), "image_url": u }))
        },
        stored: Some("SELECT COUNT(*) FROM slab_inventory WHERE url = ?"),
        takes_legacy: true,
    },
    Taker {
        name: "create sink",
        folder: "sinks",
        session: sessions::ADMIN,
        request: |s, u, n| s.post("/v1/sinks").json(&sink_body(u, n)),
        stored: Some("SELECT COUNT(*) FROM sink_type WHERE url = ?"),
        takes_legacy: true,
    },
    Taker {
        name: "update sink",
        folder: "sinks",
        session: sessions::ADMIN,
        request: |s, u, n| s.put("/v1/sinks/3100").json(&sink_body(u, n)),
        stored: Some("SELECT COUNT(*) FROM sink_type WHERE url = ?"),
        takes_legacy: true,
    },
    Taker {
        name: "sink photos",
        folder: "sinks",
        session: sessions::ADMIN,
        request: |s, u, _| {
            s.post("/v1/sinks/3100/images")
                .json(&json!({ "urls": [u] }))
        },
        stored: Some("SELECT COUNT(*) FROM installed_sinks WHERE url = ?"),
        takes_legacy: true,
    },
    Taker {
        name: "create faucet",
        folder: "faucets",
        session: sessions::ADMIN,
        request: |s, u, n| s.post("/v1/faucets").json(&faucet_body(u, n)),
        stored: Some("SELECT COUNT(*) FROM faucet_type WHERE url = ?"),
        takes_legacy: true,
    },
    Taker {
        name: "update faucet",
        folder: "faucets",
        session: sessions::ADMIN,
        request: |s, u, n| s.put("/v1/faucets/3200").json(&faucet_body(u, n)),
        stored: Some("SELECT COUNT(*) FROM faucet_type WHERE url = ?"),
        takes_legacy: true,
    },
    Taker {
        name: "faucet photos",
        folder: "faucets",
        session: sessions::ADMIN,
        request: |s, u, _| {
            s.post("/v1/faucets/3200/images")
                .json(&json!({ "urls": [u] }))
        },
        stored: Some("SELECT COUNT(*) FROM installed_faucets WHERE url = ?"),
        takes_legacy: true,
    },
    Taker {
        name: "image library",
        folder: "images",
        session: sessions::ADMIN,
        request: |s, u, n| {
            s.post("/v1/images")
                .json(&json!({ "name": format!("Island {n}"), "url": u }))
        },
        stored: Some("SELECT COUNT(*) FROM images WHERE url = ?"),
        takes_legacy: true,
    },
    Taker {
        name: "image library update",
        folder: "images",
        session: sessions::ADMIN,
        request: |s, u, _| s.patch("/v1/images/97001").json(&json!({ "url": u })),
        stored: Some("SELECT COUNT(*) FROM images WHERE url = ?"),
        takes_legacy: true,
    },
    Taker {
        name: "supports",
        folder: "supports",
        session: sessions::ADMIN,
        request: |s, u, n| {
            s.post("/v1/supports")
                .json(&json!({ "name": format!("Bracket {n}"), "url": u }))
        },
        stored: Some("SELECT COUNT(*) FROM supports WHERE url = ?"),
        takes_legacy: true,
    },
    Taker {
        name: "supports update",
        folder: "supports",
        session: sessions::ADMIN,
        request: |s, u, n| {
            s.put("/v1/supports/97002")
                .json(&json!({ "name": format!("Bracket {n}"), "url": u }))
        },
        stored: Some("SELECT COUNT(*) FROM supports WHERE url = ?"),
        takes_legacy: true,
    },
    Taker {
        name: "documents",
        folder: "documents",
        session: sessions::ADMIN,
        request: |s, u, n| {
            s.post("/v1/documents")
                .json(&json!({ "name": format!("Warranty {n}"), "url": u }))
        },
        stored: Some("SELECT COUNT(*) FROM documents WHERE url = ?"),
        takes_legacy: true,
    },
    Taker {
        name: "documents update",
        folder: "documents",
        session: sessions::ADMIN,
        request: |s, u, n| {
            s.put("/v1/documents/97003")
                .json(&json!({ "name": format!("Warranty {n}"), "url": u }))
        },
        stored: Some("SELECT COUNT(*) FROM documents WHERE url = ?"),
        takes_legacy: true,
    },
    Taker {
        name: "event photos",
        folder: "events",
        session: sessions::REP,
        request: |s, u, _| {
            s.post("/v1/events/8950/photos")
                .json(&json!({ "urls": [u] }))
        },
        stored: Some("SELECT COUNT(*) FROM event_images WHERE url = ?"),
        takes_legacy: true,
    },
    Taker {
        name: "event attachments",
        folder: "events",
        session: sessions::REP,
        request: |s, u, _| {
            s.post("/v1/events/8950/attachments")
                .json(&json!({ "files": [{ "url": u, "name": "site.png" }] }))
        },
        stored: Some("SELECT COUNT(*) FROM event_attachments WHERE url = ?"),
        takes_legacy: true,
    },
    Taker {
        name: "deal files",
        folder: "deals",
        session: sessions::DEALER,
        request: |s, u, _| {
            s.post("/v1/deals/8601/files")
                .json(&json!({ "files": [{ "url": u, "name": "site.png" }] }))
        },
        stored: Some(
            "SELECT (SELECT COUNT(*) FROM deals_images WHERE image_url = ?) + (SELECT COUNT(*) FROM deals_documents WHERE image_url = ?)",
        ),
        takes_legacy: true,
    },
    Taker {
        name: "supplier files",
        folder: "supplier-files",
        session: sessions::ADMIN,
        request: |s, u, n| {
            s.post("/v1/suppliers/301/files")
                .json(&json!({ "name": format!("List {n}"), "url": u }))
        },
        stored: Some("SELECT COUNT(*) FROM supplier_files WHERE url = ?"),
        takes_legacy: true,
    },
    Taker {
        name: "company logo",
        folder: "company-logos",
        session: sessions::ADMIN,
        request: |s, u, _| {
            s.put("/v1/company/settings")
                .json(&json!({ "name": "Acme Stone", "logo_url": u, "logo_height": 48 }))
        },
        stored: Some("SELECT COUNT(*) FROM company WHERE logo_url = ?"),
        takes_legacy: true,
    },
    Taker {
        name: "superuser company logo",
        folder: "company-logos",
        session: sessions::ADMIN,
        request: |s, u, _| {
            s.put("/v1/superuser/companies/100")
                .json(&json!({ "name": "Acme Stone", "subdomain": "acme", "max_users": 10, "logo_url": u }))
        },
        stored: Some("SELECT COUNT(*) FROM company WHERE logo_url = ?"),
        takes_legacy: true,
    },
    Taker {
        name: "bug report",
        folder: "bug-reports",
        session: sessions::REP,
        request: |s, u, _| {
            s.post("/v1/bug-reports")
                .json(&json!({ "description": "It broke", "file_urls": [u] }))
        },
        stored: Some("SELECT COUNT(*) FROM bug_report_images WHERE url = ?"),
        takes_legacy: true,
    },
    Taker {
        name: "email template attachment",
        folder: "email-templates",
        session: sessions::ADMIN,
        request: |s, u, _| {
            s.post("/v1/email-templates/8670/attachments")
                .json(&json!({ "url": u, "filename": "terms.png", "content_type": "image/png" }))
        },
        stored: Some("SELECT COUNT(*) FROM email_template_attachments WHERE url = ?"),
        takes_legacy: true,
    },
    // Trimming overwrites the file it is given.
    Taker {
        name: "quote form image",
        folder: "quote-forms",
        session: sessions::ADMIN,
        request: |s, u, _| {
            s.post("/v1/quote-forms/images")
                .json(&json!({ "url": u, "trim_border": true }))
        },
        stored: None,
        takes_legacy: false,
    },
    // The picked file is deleted once copied.
    Taker {
        name: "sms attachment",
        folder: "sms-uploads",
        session: sessions::REP,
        request: |s, u, _| {
            s.post("/v1/sms/attachments")
                .json(&json!({ "files": [{ "url": u, "filename": "a.png" }] }))
        },
        stored: None,
        takes_legacy: false,
    },
];

/// Records the takers update (`97001`-`97003`), the event, and the admin
/// as a manager and super user (company settings and records).
async fn takers_fixture(pool: &MySqlPool) {
    sqlx::raw_sql(EVENT).execute(pool).await.unwrap();
    sqlx::raw_sql(
        "INSERT INTO images (id, name, url, company_id) VALUES (97001, 'Island', 'https://img.example/island.png', 100);
         INSERT INTO supports (id, name, url, company_id) VALUES (97002, 'Bracket', 'https://img.example/bracket.png', 100);
         INSERT INTO documents (id, name, url, company_id) VALUES (97003, 'Warranty', 'https://img.example/warranty.pdf', 100);
         -- Company settings are for managers, company records for super users.
         INSERT INTO users_positions (user_id, position_id, company_id) VALUES (102, 5, 100);
         UPDATE users SET is_superuser = 1 WHERE id = 102;",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn stored_count(pool: &MySqlPool, taker: &Taker, url: &str) -> Option<i64> {
    let sql = taker.stored?;
    let mut query = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()));
    for _ in 0..sql.matches('?').count() {
        query = query.bind(url);
    }
    Some(query.fetch_one(pool).await.unwrap())
}

/// Whether the route took the file: a 2xx, and for texts a prepared file.
async fn took(res: &TestResponse, taker: &Taker) -> bool {
    if !res.status_code().is_success() {
        return false;
    }
    if taker.folder == "sms-uploads" {
        return res.json::<Value>()["uploaded"]
            .as_array()
            .is_some_and(|files| !files.is_empty());
    }
    true
}

/// Every route that takes a file URL refuses another company's file
/// (`<folder>/101/...`, which really is in the bucket), writes nothing, and
/// leaves that file and its thumbnail as they were. Its own fresh uploads
/// still go through, and older uploads whose key names no company are taken
/// where they were before, except by the routes that would overwrite or
/// delete them.
#[test_db::test(seed)]
async fn routes_take_only_the_companys_files(pool: MySqlPool) {
    takers_fixture(&pool).await;
    let (server, storage) = server_with_storage(pool.clone());
    let mut n = 0;
    for taker in TAKERS {
        let theirs = format!("{}/101/theirs-{n}.png", taker.folder);
        stored(&storage, &theirs).await;
        n += 1;
        let res = (taker.request)(&server, &url(&theirs), n)
            .with_session(taker.session)
            .await;
        assert!(
            !took(&res, taker).await,
            "{}: took another company's file: {}",
            taker.name,
            res.text()
        );
        if taker.folder != "sms-uploads" {
            assert_eq!(res.status_code(), StatusCode::BAD_REQUEST, "{}", taker.name);
        }
        assert_eq!(
            stored_count(&pool, taker, &url(&theirs)).await.unwrap_or(0),
            0,
            "{}: wrote another company's file",
            taker.name
        );
        assert!(
            intact(&storage, &theirs).await,
            "{}: changed their file",
            taker.name
        );

        let legacy = format!("{}/legacy-{n}.png", taker.folder);
        stored(&storage, &legacy).await;
        n += 1;
        let res = (taker.request)(&server, &url(&legacy), n)
            .with_session(taker.session)
            .await;
        assert_eq!(
            took(&res, taker).await,
            taker.takes_legacy,
            "{}: older file: {} {}",
            taker.name,
            res.status_code(),
            res.text()
        );
        if !taker.takes_legacy {
            assert!(
                intact(&storage, &legacy).await,
                "{}: changed an older file",
                taker.name
            );
        }

        let own = upload(&server, &storage, taker.session, taker.folder).await;
        assert!(
            own.starts_with(&url(&format!("{}/100/", taker.folder))),
            "{own}"
        );
        n += 1;
        let res = (taker.request)(&server, &own, n)
            .with_session(taker.session)
            .await;
        assert!(
            took(&res, taker).await,
            "{}: refused the company's own upload: {} {}",
            taker.name,
            res.status_code(),
            res.text()
        );
        if let Some(rows) = stored_count(&pool, taker, &own).await {
            assert_eq!(rows, 1, "{}: did not keep the own file", taker.name);
        }
    }
}

/// The thumbnail is made from the company's own upload, as before.
#[test_db::test(seed)]
async fn own_uploads_still_get_their_thumbnail(pool: MySqlPool) {
    let (server, storage) = server_with_storage(pool);
    let own = upload(&server, &storage, sessions::ADMIN, "stones").await;
    server
        .post("/v1/stones/3000/images")
        .with_session(sessions::ADMIN)
        .json(&json!({ "urls": [own] }))
        .await
        .assert_status(StatusCode::CREATED);
    let key = storage.key_for_url(&own).unwrap();
    assert!(storage.memory_keys().contains(&icon_key(&key)));
}

/// A route that deletes a record's file (or replaces it with a new one).
struct Dropper {
    name: &'static str,
    folder: &'static str,
    session: &'static str,
    /// Puts a record of company 100 with the URL (`?`) in the database.
    plant: &'static str,
    /// Deletes or replaces it: the planted id, and a fresh own upload.
    request: fn(&TestServer, u64, &str) -> TestRequest,
}

const DROPPERS: &[Dropper] = &[
    Dropper {
        name: "delete stone photo",
        folder: "stones",
        session: sessions::ADMIN,
        plant: "INSERT INTO installed_stones (url, stone_id) VALUES (?, 3000)",
        request: |s, id, _| s.delete(&format!("/v1/stones/3000/images/{id}")),
    },
    Dropper {
        name: "replace stone image",
        folder: "stones",
        session: sessions::ADMIN,
        plant: "UPDATE stones SET url = ? WHERE id = 3000",
        request: |s, _, new| s.put("/v1/stones/3000").json(&stone_body(new, 0)),
    },
    Dropper {
        name: "delete sink photo",
        folder: "sinks",
        session: sessions::ADMIN,
        plant: "INSERT INTO installed_sinks (url, sink_id) VALUES (?, 3100)",
        request: |s, id, _| s.delete(&format!("/v1/sinks/3100/images/{id}")),
    },
    Dropper {
        name: "replace sink image",
        folder: "sinks",
        session: sessions::ADMIN,
        plant: "UPDATE sink_type SET url = ? WHERE id = 3100",
        request: |s, _, new| s.put("/v1/sinks/3100").json(&sink_body(new, 0)),
    },
    Dropper {
        name: "delete faucet photo",
        folder: "faucets",
        session: sessions::ADMIN,
        plant: "INSERT INTO installed_faucets (url, faucet_id) VALUES (?, 3200)",
        request: |s, id, _| s.delete(&format!("/v1/faucets/3200/images/{id}")),
    },
    Dropper {
        name: "replace faucet image",
        folder: "faucets",
        session: sessions::ADMIN,
        plant: "UPDATE faucet_type SET url = ? WHERE id = 3200",
        request: |s, _, new| s.put("/v1/faucets/3200").json(&faucet_body(new, 0)),
    },
    Dropper {
        name: "replace library image",
        folder: "images",
        session: sessions::ADMIN,
        plant: "INSERT INTO images (name, url, company_id) VALUES ('Planted', ?, 100)",
        request: |s, id, new| {
            s.patch(&format!("/v1/images/{id}"))
                .json(&json!({ "url": new }))
        },
    },
    Dropper {
        name: "replace support",
        folder: "supports",
        session: sessions::ADMIN,
        plant: "INSERT INTO supports (name, url, company_id) VALUES ('Planted', ?, 100)",
        request: |s, id, new| {
            s.put(&format!("/v1/supports/{id}"))
                .json(&json!({ "name": "Planted", "url": new }))
        },
    },
    Dropper {
        name: "replace document",
        folder: "documents",
        session: sessions::ADMIN,
        plant: "INSERT INTO documents (name, url, company_id) VALUES ('Planted', ?, 100)",
        request: |s, id, new| {
            s.put(&format!("/v1/documents/{id}"))
                .json(&json!({ "name": "Planted", "url": new }))
        },
    },
    Dropper {
        name: "delete event photo",
        folder: "events",
        session: sessions::REP,
        plant: "INSERT INTO event_images (event_id, company_id, url, created_by) VALUES (8950, 100, ?, 'Rita Rep')",
        request: |s, id, _| s.delete(&format!("/v1/events/8950/photos/{id}")),
    },
    Dropper {
        name: "delete event attachment",
        folder: "events",
        session: sessions::REP,
        plant: "INSERT INTO event_attachments (event_id, company_id, url, name, kind, created_by) VALUES (8950, 100, ?, 'a.png', 'image', 'Rita Rep')",
        request: |s, id, _| s.delete(&format!("/v1/events/8950/attachments/{id}")),
    },
    Dropper {
        name: "delete deal image",
        folder: "deals",
        session: sessions::DEALER,
        plant: "INSERT INTO deals_images (deal_id, image_url) VALUES (8601, ?)",
        request: |s, id, _| s.delete(&format!("/v1/deals/8601/files/images/{id}")),
    },
    Dropper {
        name: "delete deal document",
        folder: "deals",
        session: sessions::DEALER,
        plant: "INSERT INTO deals_documents (deal_id, image_url, name) VALUES (8601, ?, 'a.png')",
        request: |s, id, _| s.delete(&format!("/v1/deals/8601/files/documents/{id}")),
    },
    Dropper {
        name: "delete supplier file",
        folder: "supplier-files",
        session: sessions::ADMIN,
        plant: "INSERT INTO supplier_files (name, supplier_id, url, status) VALUES ('Planted', 301, ?, 'pending')",
        request: |s, id, _| s.delete(&format!("/v1/supplier-files/{id}")),
    },
];

/// Plants a record holding `url`; its id (0 for an update).
async fn plant(pool: &MySqlPool, sql: &str, url: &str) -> u64 {
    sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
        .bind(url)
        .execute(pool)
        .await
        .unwrap()
        .last_insert_id()
}

/// Records planted before the fix (a company 100 record holding another
/// company's URL, `stones/101/...`, or a neighbour's `stones/1000/...`):
/// deleting or replacing them through any route leaves that company's file
/// and thumbnail in the bucket. Older keys that name no company are kept
/// too (another company's record may hold the same URL). The company's own
/// files are still deleted with their thumbnail, as before.
#[test_db::test(seed)]
async fn deleting_a_record_deletes_only_the_companys_file(pool: MySqlPool) {
    sqlx::raw_sql(EVENT).execute(&pool).await.unwrap();
    let (server, storage) = server_with_storage(pool.clone());
    let mut n = 0;
    for dropper in DROPPERS {
        for (owner, deleted) in [
            ("101/", false),
            ("1000/", false),
            ("", false),
            ("100/", true),
        ] {
            n += 1;
            let key = format!("{}/{owner}planted-{n}.png", dropper.folder);
            stored(&storage, &key).await;
            let id = plant(&pool, dropper.plant, &url(&key)).await;
            let new = upload(&server, &storage, dropper.session, dropper.folder).await;
            let res = (dropper.request)(&server, id, &new)
                .with_session(dropper.session)
                .await;
            assert!(
                res.status_code().is_success(),
                "{} ({key}): {} {}",
                dropper.name,
                res.status_code(),
                res.text()
            );
            if deleted {
                assert!(
                    gone(&storage, &key).await,
                    "{}: kept the own file {key}",
                    dropper.name
                );
            } else {
                assert!(
                    intact(&storage, &key).await,
                    "{}: deleted {key}",
                    dropper.name
                );
            }
        }
    }
}

/// Deleting a supplier deletes its own price files, never another
/// company's planted on it.
#[test_db::test(seed)]
async fn deleting_a_supplier_deletes_only_the_companys_files(pool: MySqlPool) {
    let (server, storage) = server_with_storage(pool.clone());
    sqlx::raw_sql(
        "INSERT INTO suppliers (id, supplier_name, company_id) VALUES (97301, 'Planted Co', 100)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let theirs = "supplier-files/101/list.png";
    let legacy = "supplier-files/list.png";
    let own = "supplier-files/100/list.png";
    for key in [theirs, legacy, own] {
        stored(&storage, key).await;
        plant(
            &pool,
            "INSERT INTO supplier_files (name, supplier_id, url, status) VALUES ('Planted', 97301, ?, 'pending')",
            &url(key),
        )
        .await;
    }
    server
        .delete("/v1/suppliers/97301")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert!(intact(&storage, theirs).await);
    assert!(intact(&storage, legacy).await);
    assert!(gone(&storage, own).await);
}

/// URLs that only look like the company's file are refused: a neighbour
/// whose id starts with the company's (`1000` for `100`), another folder,
/// a path escaping with `..`, a folder with no file, another host,
/// percent-encoded slashes and a different case. Each file (but the empty
/// one) is in the bucket, so only the key check stops it.
#[test_db::test(seed)]
async fn lookalike_keys_are_refused(pool: MySqlPool) {
    let (server, storage) = server_with_storage(pool.clone());
    for key in [
        "stones/1000/x.png",
        "stones/10/x.png",
        "sinks/100/x.png",
        "stones/101/x.png",
        "stones/100%2F..%2F101%2Fx.png",
        "stones%2F100%2Fx.png",
        "Stones/100/x.png",
        "stones-100/x.png",
        "stones/x/y.png",
    ] {
        stored(&storage, key).await;
    }
    for candidate in [
        url("stones/1000/x.png"),
        url("stones/10/x.png"),
        url("sinks/100/x.png"),
        url("stones/100/../101/x.png"),
        url("stones/100/"),
        url("stones/100%2F..%2F101%2Fx.png"),
        url("stones%2F100%2Fx.png"),
        url("Stones/100/x.png"),
        url("stones-100/x.png"),
        url("stones/x/y.png"),
        "https://elsewhere.test/bucket/stones/100/x.png".to_string(),
        format!("{MEMORY_PUBLIC_BASE}x/stones/100/x.png"),
        "stones/100/x.png".to_string(),
    ] {
        let res = server
            .post("/v1/stones/3000/images")
            .with_session(sessions::ADMIN)
            .json(&json!({ "urls": [candidate] }))
            .await;
        assert_eq!(res.status_code(), StatusCode::BAD_REQUEST, "{candidate}");
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM installed_stones WHERE url = ?",
                &candidate
            )
            .await,
            0,
            "{candidate}"
        );
    }
    assert!(intact(&storage, "stones/1000/x.png").await);
    assert!(intact(&storage, "stones/101/x.png").await);
}

/// The key checks themselves, for the cases a route cannot reach.
#[test]
fn key_checks_compare_whole_segments() {
    let storage = Storage::memory();
    let stones = UploadFolder::Stones;
    assert_eq!(
        company_key(&storage, &url("stones/100/a.png"), stones, 100).as_deref(),
        Some("stones/100/a.png")
    );
    for key in [
        "stones/1000/a.png",
        "stones/10/a.png",
        "stones/100a.png",
        "sinks/100/a.png",
        "stones/101/a.png",
    ] {
        assert_eq!(company_key(&storage, &url(key), stones, 100), None, "{key}");
    }
    // Deleting checks the company segment of any folder.
    for key in [
        "stones/1000/a.png",
        "stones/10/a.png",
        "stones/100a.png",
        "stones/101/a.png",
    ] {
        assert!(!owned_by(&storage, &url(key), 100), "{key}");
    }
    assert!(owned_by(&storage, &url("sinks/100/a.png"), 100));
    assert_eq!(
        accepted_key(&storage, &url("stones/legacy.png"), stones, 100).as_deref(),
        Some("stones/legacy.png")
    );
    for key in [
        "stones/101/a.png",
        "stones/1000/a.png",
        "stones/",
        "stones",
        "sinks/legacy.png",
        "stones/100/../101/a.png",
    ] {
        assert_eq!(
            accepted_key(&storage, &url(key), stones, 100),
            None,
            "{key}"
        );
    }
    assert!(owned_by(
        &storage,
        &url("documents/100/a-preview.webp"),
        100
    ));
    assert!(owned_by(&storage, &url("sms-attachments/100/m/a.jpg"), 100));
    for key in [
        "stones/100/",
        "stones/100",
        "/100/a.png",
        "stones/legacy.png",
    ] {
        assert!(!owned_by(&storage, &url(key), 100), "{key}");
    }
    assert!(!owned_by(
        &storage,
        "https://elsewhere.test/stones/100/a.png",
        100
    ));
}

/// Folders and who may upload to them (`POST /v1/uploads`).
const UPLOAD_FOLDERS: [(&str, &str); 18] = [
    ("stones", sessions::ADMIN),
    ("sinks", sessions::ADMIN),
    ("faucets", sessions::ADMIN),
    ("deals", sessions::REP),
    ("emails", sessions::REP),
    ("events", sessions::REP),
    ("quotes", sessions::REP),
    ("template-images", sessions::REP),
    ("email-templates", sessions::ADMIN),
    ("documents", sessions::ADMIN),
    ("images", sessions::ADMIN),
    ("supports", sessions::ADMIN),
    ("bug-reports", sessions::REP),
    ("leads", sessions::ADMIN),
    ("sms-uploads", sessions::REP),
    ("supplier-files", sessions::ADMIN),
    ("company-logos", sessions::ADMIN),
    ("quote-forms", sessions::ADMIN),
];

/// `<folder>/<company id>/<uuid>.<ext>`, with a uuid that names one upload.
fn assert_company_key(url: &str, folder: &str, company_id: i32) {
    let key = url
        .strip_prefix(&format!("{MEMORY_PUBLIC_BASE}/"))
        .unwrap_or_else(|| panic!("{url}"));
    let rest = key
        .strip_prefix(&format!("{folder}/{company_id}/"))
        .unwrap_or_else(|| panic!("{key} is not under {folder}/{company_id}/"));
    let (name, ext) = rest.rsplit_once('.').unwrap();
    assert_eq!(ext, "png", "{key}");
    assert!(uuid::Uuid::parse_str(name).is_ok(), "{key}");
}

async fn upload_url(request: TestRequest, folder: &str) -> Value {
    let res = request
        .json(&json!({ "folder": folder, "file_name": "a.PNG", "content_type": "image/png" }))
        .await;
    res.assert_status(StatusCode::CREATED);
    res.json()
}

/// Every folder's uploads land under the uploader's company: company 100
/// for its users, 101 for the other company's rep, and for a super admin
/// the company they act in. Two uploads never share a key, and the email
/// token signs the URL with its company.
#[test_db::test(seed)]
async fn uploads_land_under_the_uploaders_company(pool: MySqlPool) {
    sqlx::query("UPDATE users SET is_admin = 1 WHERE id = 106")
        .execute(&pool)
        .await
        .unwrap();
    let (server, _) = server_with_storage(pool);
    for (folder, session) in UPLOAD_FOLDERS {
        let first = upload_url(server.post("/v1/uploads").with_session(session), folder).await;
        let second = upload_url(server.post("/v1/uploads").with_session(session), folder).await;
        let first_url = first["url"].as_str().unwrap();
        assert_company_key(first_url, folder, 100);
        assert_ne!(first_url, second["url"].as_str().unwrap(), "{folder}");
        let theirs = upload_url(
            server
                .post("/v1/uploads")
                .with_session(sessions::OTHER_COMPANY_REP),
            folder,
        )
        .await;
        assert_company_key(theirs["url"].as_str().unwrap(), folder, 101);
        if folder == "emails" {
            let user_id = if session == sessions::REP { 100 } else { 102 };
            assert!(attachment_token::verify(
                SECRET,
                first_url,
                user_id,
                first["token"].as_str()
            ));
        }
        for company_id in [100, 101] {
            let acting = cookie_for(SessionData {
                session_id: Some(sessions::SUPER_ADMIN.to_string()),
                active_company_id: Some(company_id),
            });
            let upload = upload_url(
                server.post("/v1/uploads").add_header("cookie", acting),
                folder,
            )
            .await;
            assert_company_key(upload["url"].as_str().unwrap(), folder, company_id);
        }
    }
}

/// Lead pictures go under the company of the marketing team the upload is
/// for, and only its team (or a super admin) may upload them.
#[test_db::test(seed)]
async fn lead_pictures_land_under_the_teams_company(pool: MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (100, 7, 100);
         UPDATE users SET is_superuser = 1 WHERE id = 103;",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (server, _) = server_with_storage(pool);
    let lead = |company_id: i32| format!("/v1/marketing/{company_id}/uploads");
    let body = json!({ "file_name": "lead.png", "content_type": "image/png" });
    let res = server
        .post(&lead(100))
        .with_session(sessions::REP)
        .json(&body)
        .await;
    res.assert_status(StatusCode::CREATED);
    assert_company_key(res.json::<Value>()["url"].as_str().unwrap(), "leads", 100);
    server
        .post(&lead(101))
        .with_session(sessions::REP)
        .json(&body)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    // A super user acting in company 100 uploads for company 101's team.
    let acting = cookie_for(SessionData {
        session_id: Some(sessions::SUPER_ADMIN.to_string()),
        active_company_id: Some(100),
    });
    let res = server
        .post(&lead(101))
        .add_header("cookie", acting)
        .json(&body)
        .await;
    res.assert_status(StatusCode::CREATED);
    assert_company_key(res.json::<Value>()["url"].as_str().unwrap(), "leads", 101);
}
