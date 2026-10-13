//! Pictures made for the browser: PDF first pages, email attachments (HEIC
//! too) and files picked for a text.

mod common;

use api::storage::{MEMORY_PUBLIC_BASE, Storage};
use axum::http::StatusCode;
use common::{WithSession, server_with_storage, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

const HEIC: &[u8] = include_bytes!("fixtures/photo-640x480.heic");
const HEIC_ROTATED: &[u8] = include_bytes!("fixtures/photo-rotated-480x640.heic");
const HEIC_GRID: &[u8] = include_bytes!("fixtures/photo-4032x3024-grid.heic");

fn url(key: &str) -> String {
    format!("{MEMORY_PUBLIC_BASE}/{key}")
}

fn a_pdf() -> Vec<u8> {
    let mut page = api::pdf::Page::new();
    page.text("Install agreement", 72.0, 700.0, 28.0, api::pdf::Font::Bold);
    page.finish()
}

fn decoded(bytes: &[u8]) -> image::DynamicImage {
    image::load_from_memory(bytes).expect("a picture")
}

fn location(response: &axum_test::TestResponse) -> String {
    response
        .headers()
        .get("location")
        .expect("a redirect")
        .to_str()
        .unwrap()
        .to_string()
}

// ---- GET /v1/documents/{id}/preview

#[test_db::test(seed)]
async fn a_pdf_documents_first_page_is_drawn_once(pool: MySqlPool) {
    let (server, storage) = server_with_storage(pool.clone());
    storage
        .put("documents/agreement.pdf", a_pdf(), "application/pdf")
        .await
        .unwrap();
    storage
        .put(
            "documents/broken.pdf",
            b"%PDF-1.7 broken".to_vec(),
            "application/pdf",
        )
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO documents (id, name, url, company_id) VALUES
           (9900, 'Agreement', ?, 100), (9901, 'Logo', ?, 100), (9902, 'Theirs', ?, 101),
           (9903, 'Broken', ?, 100)",
    )
    .bind(url("documents/agreement.pdf"))
    .bind(url("documents/logo.png"))
    .bind(url("documents/agreement.pdf"))
    .bind(url("documents/broken.pdf"))
    .execute(&pool)
    .await
    .unwrap();

    let response = server
        .get("/v1/documents/9900/preview")
        .with_session(sessions::REP)
        .await;
    response.assert_status(StatusCode::FOUND);
    assert_eq!(location(&response), url("documents/agreement-preview.webp"));
    assert_eq!(
        response.headers()["cache-control"],
        "private, max-age=86400"
    );
    let stored = storage
        .get("documents/agreement-preview.webp")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.content_type, "image/webp");
    assert_eq!(decoded(&stored.bytes).width(), 400);

    // Drawn once: a later view finds the stored picture.
    storage
        .put(
            "documents/agreement-preview.webp",
            b"cached".to_vec(),
            "image/webp",
        )
        .await
        .unwrap();
    server
        .get("/v1/documents/9900/preview")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FOUND);
    assert_eq!(
        storage
            .get("documents/agreement-preview.webp")
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"cached"
    );

    for id in [9901, 9902, 9903, 99_999] {
        server
            .get(&format!("/v1/documents/{id}/preview"))
            .with_session(sessions::REP)
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }
    assert!(
        !storage
            .exists("documents/broken-preview.webp")
            .await
            .unwrap()
    );
    server
        .get("/v1/documents/9900/preview")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

// ---- GET /v1/email-attachments/{id}/preview

async fn attachments(pool: &MySqlPool, storage: &Storage) {
    storage
        .put("emails/photo.heic", HEIC.to_vec(), "image/heic")
        .await
        .unwrap();
    storage
        .put("emails/rotated.heic", HEIC_ROTATED.to_vec(), "image/heic")
        .await
        .unwrap();
    let png = {
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(900, 600)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    };
    storage
        .put("emails/plan.png", png.clone(), "image/png")
        .await
        .unwrap();
    // Named HEIC, but a PNG inside.
    storage
        .put("emails/fake.heic", png, "image/heic")
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO email_attachments (id, email_id, content_type, content_subtype, filename, url) VALUES
           (9910, 8802, 'image', 'heic', 'IMG_1.HEIC', ?),
           (9911, 8802, 'image', 'png', 'plan.png', ?),
           (9912, 8802, 'application', 'pdf', 'quote.pdf', 'https://example.test/q.pdf'),
           (9913, 8802, 'application', 'octet-stream', 'IMG_2.heic', ?),
           (9914, 8802, 'image', 'heic', 'fake.heic', ?),
           (9915, 8807, 'image', 'heic', 'acme.heic', ?)",
    )
    .bind(url("emails/photo.heic"))
    .bind(url("emails/plan.png"))
    .bind(url("emails/rotated.heic"))
    .bind(url("emails/fake.heic"))
    .bind(url("emails/photo.heic"))
    .execute(pool)
    .await
    .unwrap();
}

#[test_db::test(seed)]
async fn a_heic_attachment_becomes_a_jpeg_kept_for_later(pool: MySqlPool) {
    let (server, storage) = server_with_storage(pool.clone());
    attachments(&pool, &storage).await;
    let response = server
        .get("/v1/email-attachments/9910/preview")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await;
    response.assert_status(StatusCode::FOUND);
    let target = location(&response);
    let key = target
        .strip_prefix(&format!("{MEMORY_PUBLIC_BASE}/"))
        .unwrap();
    assert!(
        key.starts_with("email-attachment-previews/") && key.ends_with(".jpg"),
        "{key}"
    );
    let jpeg = storage.get(key).await.unwrap().unwrap();
    assert_eq!(jpeg.content_type, "image/jpeg");
    let picture = decoded(&jpeg.bytes);
    assert_eq!((picture.width(), picture.height()), (640, 480));

    // Rotated photos come out upright (taller than wide).
    let rotated = server
        .get("/v1/email-attachments/9913/preview")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await;
    let key = location(&rotated).replace(&format!("{MEMORY_PUBLIC_BASE}/"), "");
    let picture = decoded(&storage.get(&key).await.unwrap().unwrap().bytes);
    assert_eq!((picture.width(), picture.height()), (480, 640));

    // A file named HEIC that is really a PNG is shown as it is.
    let fake = server
        .get("/v1/email-attachments/9914/preview")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await;
    fake.assert_status(StatusCode::FOUND);
    assert_eq!(location(&fake), url("emails/fake.heic"));
}

#[test_db::test(seed)]
async fn other_pictures_redirect_and_icons_are_small_jpegs(pool: MySqlPool) {
    let (server, storage) = server_with_storage(pool.clone());
    attachments(&pool, &storage).await;
    let plain = server
        .get("/v1/email-attachments/9911/preview")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await;
    plain.assert_status(StatusCode::FOUND);
    assert_eq!(location(&plain), url("emails/plan.png"));

    for (id, size) in [(9911, (320, 213)), (9910, (320, 240))] {
        let icon = server
            .get(&format!("/v1/email-attachments/{id}/preview?icon=true"))
            .with_session(sessions::OTHER_COMPANY_REP)
            .await;
        icon.assert_status_ok();
        assert_eq!(icon.headers()["content-type"], "image/jpeg");
        assert_eq!(
            icon.headers()["cache-control"],
            "private, max-age=604800, immutable"
        );
        let picture = decoded(icon.as_bytes());
        assert_eq!((picture.width(), picture.height()), size, "{id}");
    }
    let thumbnails = storage
        .memory_keys()
        .into_iter()
        .filter(|key| key.starts_with("email-attachment-thumbnails/"))
        .count();
    assert_eq!(thumbnails, 2);
}

#[test_db::test(seed)]
async fn attachment_previews_are_for_the_company_and_pictures_only(pool: MySqlPool) {
    let (server, storage) = server_with_storage(pool.clone());
    attachments(&pool, &storage).await;
    // Company 100 cannot see company 101's attachment, and the other way round.
    server
        .get("/v1/email-attachments/9910/preview")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .get("/v1/email-attachments/9915/preview")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .get("/v1/email-attachments/9915/preview?icon=true")
        .with_session(sessions::REP)
        .await
        .assert_status_ok();
    let pdf = server
        .get("/v1/email-attachments/9912/preview")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await;
    pdf.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(pdf.json::<Value>(), json!({ "error": "Not an image" }));
    server
        .get("/v1/email-attachments/9910/preview")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    // The seeded attachment lives outside the bucket.
    server
        .get("/v1/email-attachments/8810/preview?icon=true")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .assert_status(StatusCode::BAD_GATEWAY);
}

// ---- POST /v1/sms/attachments

async fn upload(
    server: &axum_test::TestServer,
    storage: &Storage,
    name: &str,
    kind: &str,
    bytes: &[u8],
) -> String {
    let upload = server
        .post("/v1/uploads")
        .with_session(sessions::REP)
        .json(&json!({ "folder": "sms-uploads", "file_name": name, "content_type": kind }))
        .await
        .json::<Value>();
    let public = upload["url"].as_str().unwrap().to_string();
    let key = storage.key_for_url(&public).unwrap();
    assert!(key.starts_with("sms-uploads/"), "{key}");
    storage.put(&key, bytes.to_vec(), kind).await.unwrap();
    public
}

#[test_db::test(seed)]
async fn picked_files_are_made_ready_to_text(pool: MySqlPool) {
    let (server, storage) = server_with_storage(pool);
    let heic = upload(&server, &storage, "IMG_7.HEIC", "image/heic", HEIC_GRID).await;
    let pdf = upload(
        &server,
        &storage,
        "Quote.pdf",
        "application/pdf",
        b"%PDF-1.4 quote",
    )
    .await;
    // A browser that could not type the file: the name decides.
    let csv = upload(
        &server,
        &storage,
        "list.csv",
        "application/octet-stream",
        b"a,b\n1,2\n",
    )
    .await;
    let junk = upload(
        &server,
        &storage,
        "notes.bin",
        "application/octet-stream",
        b"\x00\x01junk",
    )
    .await;
    let body = server
        .post("/v1/sms/attachments")
        .with_session(sessions::REP)
        .json(&json!({ "files": [
            { "url": heic, "filename": "IMG_7.HEIC" },
            { "url": pdf, "filename": "Quote.pdf" },
            { "url": csv, "filename": "list.csv" },
            { "url": junk, "filename": "notes.bin" },
            { "url": "https://example.test/x.jpg", "filename": "x.jpg" },
            { "url": url("sms-uploads/missing.jpg"), "filename": "gone.jpg" },
            { "url": url("stones/a.jpg"), "filename": "a.jpg" },
        ] }))
        .await
        .json::<Value>();

    let uploaded = body["uploaded"].as_array().unwrap();
    assert_eq!(uploaded.len(), 3);
    let photo = &uploaded[0];
    assert_eq!(photo["content_type"], "image/jpeg");
    assert_eq!(photo["filename"], "IMG_7.HEIC");
    assert_eq!(
        (photo["width"].as_u64(), photo["height"].as_u64()),
        (Some(1600), Some(1200))
    );
    let key = photo["s3_key"].as_str().unwrap();
    assert!(
        key.starts_with("sms-attachments/100/") && key.ends_with(".jpg"),
        "{key}"
    );
    assert_eq!(photo["url"], url(key));
    let stored = storage.get(key).await.unwrap().unwrap();
    assert_eq!(photo["size"].as_u64(), Some(stored.bytes.len() as u64));
    assert!(stored.bytes.len() <= 480 * 1024);
    assert_eq!(decoded(&stored.bytes).width(), 1600);

    assert_eq!(uploaded[1]["content_type"], "application/pdf");
    assert_eq!(uploaded[1]["width"], Value::Null);
    assert!(uploaded[1]["s3_key"].as_str().unwrap().ends_with(".pdf"));
    assert_eq!(uploaded[2]["content_type"], "text/csv");
    // One text, one folder.
    let folder = |v: &Value| {
        v["s3_key"]
            .as_str()
            .unwrap()
            .rsplit_once('/')
            .unwrap()
            .0
            .to_string()
    };
    assert_eq!(folder(&uploaded[0]), folder(&uploaded[1]));

    assert_eq!(
        body["failed"],
        json!([
            { "filename": "notes.bin", "error": "unsupported_image" },
            { "filename": "x.jpg", "error": "invalid_upload" },
            { "filename": "gone.jpg", "error": "upload_missing" },
            { "filename": "a.jpg", "error": "invalid_upload" },
        ])
    );
    // The prepared uploads are gone; the unreadable one is left for the bucket's expiry.
    let leftovers: Vec<String> = storage
        .memory_keys()
        .into_iter()
        .filter(|key| key.starts_with("sms-uploads/"))
        .collect();
    assert_eq!(leftovers.len(), 1);
}

#[test_db::test(seed)]
async fn picked_files_need_one_to_ten_files_and_a_session(pool: MySqlPool) {
    let (server, _) = server_with_storage(pool);
    let empty = server
        .post("/v1/sms/attachments")
        .with_session(sessions::REP)
        .json(&json!({ "files": [] }))
        .await;
    empty.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(empty.json::<Value>(), json!({ "error": "no_images" }));
    let files: Vec<Value> = (0..11)
        .map(|i| json!({ "url": url(&format!("sms-uploads/{i}.jpg")), "filename": "x.jpg" }))
        .collect();
    let many = server
        .post("/v1/sms/attachments")
        .with_session(sessions::REP)
        .json(&json!({ "files": files }))
        .await;
    many.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(many.json::<Value>(), json!({ "error": "too_many_images" }));
    server
        .post("/v1/sms/attachments")
        .json(&json!({ "files": [] }))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    server
        .post("/v1/uploads")
        .with_session(sessions::REP)
        .json(&json!({ "folder": "sms-uploads", "file_name": "x.exe", "content_type": "application/x-msdownload" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

// ---- GET /v1/calls/{provider}/{media_id}/recording

async fn recording_server(
    pool: MySqlPool,
) -> (
    axum_test::TestServer,
    Storage,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    use axum::extract::Path;
    use axum::routing::get;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let fetches = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&fetches);
    // Company 101 (Olga) has CloudTalk keys: call 55 is recorded, 56 is not.
    let router = axum::Router::new().route(
        "/calls/recording/{file}",
        get(move |Path(file): Path<String>| {
            let counted = Arc::clone(&counted);
            async move {
                counted.fetch_add(1, Ordering::SeqCst);
                let bytes: &[u8] = if file == "55.json" {
                    b"RIFF\0\0\0\0WAVEfmt data"
                } else {
                    b""
                };
                ([("content-type", "audio/wav")], bytes)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let storage = Storage::memory();
    let state = api::state::AppState::new(pool, common::SECRET, common::ALLOWED_ORIGIN)
        .with_storage(storage.clone())
        .with_cloudtalk_base(&base);
    (
        axum_test::TestServer::new(api::app(state)),
        storage,
        fetches,
    )
}

#[test_db::test(seed)]
async fn a_call_recording_is_copied_once_and_linked(pool: MySqlPool) {
    use std::sync::atomic::Ordering;
    let (server, storage, fetches) = recording_server(pool).await;
    let first = server
        .get("/v1/calls/cloudtalk/55/recording")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .json::<Value>();
    assert_eq!(first["content_type"], "audio/wav");
    let link = first["url"].as_str().unwrap();
    let key = link
        .strip_prefix(&format!("{MEMORY_PUBLIC_BASE}/"))
        .unwrap();
    assert!(
        key.starts_with("call-recordings/") && key.len() > 60,
        "{key}"
    );
    assert!(!key.contains("55"), "the key does not name the call: {key}");
    assert!(
        storage
            .get(key)
            .await
            .unwrap()
            .unwrap()
            .bytes
            .starts_with(b"RIFF")
    );
    let again = server
        .get("/v1/calls/cloudtalk/55/recording")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .json::<Value>();
    assert_eq!(again, first);
    assert_eq!(fetches.load(Ordering::SeqCst), 1);

    server
        .get("/v1/calls/cloudtalk/56/recording")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    for path in [
        "/v1/calls/cloudtalk/rec_55/recording",
        "/v1/calls/ringcentral/55/recording",
        "/v1/calls/ringcentral/vm_1_2/recording",
    ] {
        server
            .get(path)
            .with_session(sessions::OTHER_COMPANY_REP)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    server
        .get("/v1/calls/cloudtalk/55/recording")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

// ---- POST /v1/quotes/print-previews

#[test_db::test(seed)]
async fn a_printed_quote_gets_upload_and_open_links(pool: MySqlPool) {
    let (server, _) = server_with_storage(pool);
    let created = server
        .post("/v1/quotes/print-previews")
        .with_session(sessions::REP)
        .json(&json!({ "filename": "Smith Kitchen.pdf" }))
        .await;
    created.assert_status(StatusCode::CREATED);
    let body = created.json::<Value>();
    let upload = body["upload_url"].as_str().unwrap();
    assert!(
        upload.starts_with(&format!("{MEMORY_PUBLIC_BASE}/quote-previews/")),
        "{upload}"
    );
    assert!(body["url"].as_str().unwrap().ends_with(".pdf"));
    for name in ["notes.txt", "a/b.pdf", ""] {
        server
            .post("/v1/quotes/print-previews")
            .with_session(sessions::REP)
            .json(&json!({ "filename": name }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    server
        .post("/v1/quotes/print-previews")
        .json(&json!({ "filename": "x.pdf" }))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}
