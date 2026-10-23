//! Files a browser would run script in (HTML, SVG, XML, JavaScript) never
//! get into the bucket through `POST /v1/uploads`, are never handed back by
//! `GET /v1/uploads/file` or `GET /v1/stones/image-proxy`, and whatever is
//! handed back cannot render as a page of the API's origin.

mod common;

use axum::http::StatusCode;
use axum_test::{TestResponse, TestServer};
use common::{WithSession, sessions};
use serde_json::json;
use sqlx::MySqlPool;

const FOLDERS: [&str; 18] = [
    "stones",
    "sinks",
    "faucets",
    "deals",
    "emails",
    "events",
    "quotes",
    "template-images",
    "email-templates",
    "documents",
    "images",
    "supports",
    "bug-reports",
    "leads",
    "sms-uploads",
    "supplier-files",
    "company-logos",
    "quote-forms",
];

const SCRIPT_TYPES: [&str; 16] = [
    "image/svg+xml",
    "text/html",
    "application/xhtml+xml",
    "application/vnd.wap.xhtml+xml",
    "text/xml",
    "application/xml",
    "text/xsl",
    "application/xslt+xml",
    "application/rss+xml",
    "application/mathml+xml",
    "text/javascript",
    "text/ecmascript",
    "application/javascript",
    "application/x-javascript",
    "application/ecmascript",
    "application/x-shockwave-flash",
];

/// The folders that take any well-formed type, so only the script check
/// stands between them and HTML.
const ANY_TYPE_FOLDERS: [&str; 3] = ["emails", "bug-reports", "email-templates"];

fn urlencoding(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
}

async fn start_upload(server: &TestServer, folder: &str, content_type: &str) -> TestResponse {
    server
        .post("/v1/uploads")
        .with_session(sessions::ADMIN)
        .json(&json!({ "folder": folder, "file_name": "x", "content_type": content_type }))
        .await
}

async fn fetch_file(server: &TestServer, url: &str) -> TestResponse {
    server
        .get(&format!("/v1/uploads/file?url={}", urlencoding(url)))
        .with_session(sessions::REP)
        .await
}

async fn proxy_image(server: &TestServer, url: &str) -> TestResponse {
    server
        .get(&format!("/v1/stones/image-proxy?url={}", urlencoding(url)))
        .with_session(sessions::ADMIN)
        .await
}

/// The headers that keep a stored file from rendering as a page, each sent
/// exactly once.
fn assert_never_a_page(res: &TestResponse, content_type: &str) {
    for (name, value) in [
        ("content-type", content_type),
        ("x-content-type-options", "nosniff"),
        ("content-security-policy", "default-src 'none'; sandbox"),
        ("content-disposition", "attachment"),
        ("cache-control", "private, max-age=3600"),
    ] {
        let values: Vec<_> = res.iter_headers_by_name(name).collect();
        assert_eq!(values, [value], "{name}");
    }
}

#[test_db::test(seed)]
async fn no_folder_takes_a_type_that_runs_script(pool: MySqlPool) {
    let (server, _) = common::server_with_storage(pool);
    for folder in FOLDERS {
        for content_type in SCRIPT_TYPES {
            let res = start_upload(&server, folder, content_type).await;
            assert_eq!(
                res.status_code(),
                StatusCode::BAD_REQUEST,
                "{folder} {content_type}"
            );
        }
    }
}

#[test_db::test(seed)]
async fn script_types_are_refused_however_they_are_spelled(pool: MySqlPool) {
    let (server, _) = common::server_with_storage(pool);
    for folder in ANY_TYPE_FOLDERS {
        for content_type in [
            "IMAGE/SVG+XML",
            "Text/Html",
            "  text/html  ",
            "\ttext/html\n",
            "text/html; charset=utf-8",
            "text/html;",
            "image/svg+xml;charset=utf-8",
            "text/html ; x=y",
            "text/ html",
            "text/html,image/png",
        ] {
            let res = start_upload(&server, folder, content_type).await;
            assert_eq!(
                res.status_code(),
                StatusCode::BAD_REQUEST,
                "{folder} {content_type:?}"
            );
        }
    }
}

#[test_db::test(seed)]
async fn ordinary_files_still_upload_to_the_open_folders(pool: MySqlPool) {
    let (server, _) = common::server_with_storage(pool);
    for folder in ANY_TYPE_FOLDERS {
        for content_type in [
            "image/png",
            "image/jpeg",
            "application/pdf",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "application/zip",
            "application/json",
            "application/octet-stream",
            "text/plain",
            "text/csv",
            "text/calendar",
            "video/mp4",
        ] {
            let res = start_upload(&server, folder, content_type).await;
            assert_eq!(
                res.status_code(),
                StatusCode::CREATED,
                "{folder} {content_type}"
            );
        }
    }
}

#[test_db::test(seed)]
async fn stored_files_that_run_script_are_never_served(pool: MySqlPool) {
    // Inbound mail stores attachments with whatever type the sender gave, and
    // older uploads predate the check: the bucket may hold any of these.
    let (server, storage) = common::server_with_storage(pool);
    let stored = [
        "image/svg+xml",
        "IMAGE/SVG+XML",
        "image/svg+xml; charset=utf-8",
        " image/svg+xml ",
        "text/html",
        "text/html; charset=utf-8",
        "application/xhtml+xml",
        "text/xml",
        "application/xml",
        "application/javascript",
        "text/plain",
        "application/octet-stream",
        "",
    ];
    for (i, content_type) in stored.iter().enumerate() {
        let key = format!("emails/script-{i}");
        storage
            .put(
                &key,
                b"<svg xmlns='http://www.w3.org/2000/svg' onload='alert(1)'/>".to_vec(),
                content_type,
            )
            .await
            .unwrap();
        let url = storage.public_url(&key).unwrap();
        for res in [
            fetch_file(&server, &url).await,
            proxy_image(&server, &url).await,
        ] {
            assert_eq!(
                res.status_code(),
                StatusCode::BAD_REQUEST,
                "{content_type:?}"
            );
            assert_eq!(res.header("content-type"), "application/json");
        }
    }
}

#[test_db::test(seed)]
async fn images_and_documents_come_back_as_downloads(pool: MySqlPool) {
    let (server, storage) = common::server_with_storage(pool);
    for (i, (stored, served, by_proxy)) in [
        ("image/png", "image/png", true),
        ("image/jpeg", "image/jpeg", true),
        ("image/webp", "image/webp", true),
        ("image/gif", "image/gif", true),
        ("image/heic", "image/heic", true),
        // The served type is the bare, lowercased stored one: no parameter
        // can smuggle anything into the header.
        ("Image/PNG; name=\"a.png\"", "image/png", true),
        ("application/pdf", "application/pdf", false),
        ("application/msword", "application/msword", false),
        (
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            false,
        ),
        (
            "application/vnd.ms-excel",
            "application/vnd.ms-excel",
            false,
        ),
        (
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            false,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        // HTML bytes under an image type: a sniffing browser would render them.
        let bytes = format!("<html><script>alert({i})</script></html>").into_bytes();
        let key = format!("emails/file-{i}");
        storage.put(&key, bytes.clone(), stored).await.unwrap();
        let url = storage.public_url(&key).unwrap();

        let res = fetch_file(&server, &url).await;
        res.assert_status_ok();
        assert_never_a_page(&res, served);
        assert_eq!(res.as_bytes().as_ref(), bytes.as_slice(), "{stored}");

        let res = proxy_image(&server, &url).await;
        if by_proxy {
            res.assert_status_ok();
            assert_never_a_page(&res, served);
            assert_eq!(res.as_bytes().as_ref(), bytes.as_slice(), "{stored}");
        } else {
            assert_eq!(res.status_code(), StatusCode::BAD_REQUEST, "{stored}");
        }
    }
}

#[test_db::test(seed)]
async fn stored_files_are_for_employees_and_the_proxy_for_admins(pool: MySqlPool) {
    let (server, storage) = common::server_with_storage(pool);
    storage
        .put("emails/a.png", b"png".to_vec(), "image/png")
        .await
        .unwrap();
    let url = urlencoding(&storage.public_url("emails/a.png").unwrap());
    server
        .get(&format!("/v1/uploads/file?url={url}"))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    server
        .get(&format!("/v1/stones/image-proxy?url={url}"))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    server
        .get(&format!("/v1/stones/image-proxy?url={url}"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    // Only the bucket's own URLs, and no climbing out of it.
    for url in [
        "https://elsewhere.test/a.png",
        "javascript:alert(1)",
        &storage.public_url("emails/../emails/a.png").unwrap(),
    ] {
        fetch_file(&server, url)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
        proxy_image(&server, url)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    let missing = storage.public_url("emails/missing.png").unwrap();
    fetch_file(&server, &missing)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    proxy_image(&server, &missing)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn no_response_may_be_sniffed_into_another_type(pool: MySqlPool) {
    let server = common::server(pool);
    let responses = [
        server.get("/").await,
        server.get("/openapi.json").await,
        server.get("/v1/me").with_session(sessions::REP).await,
        server.get("/v1/me").await,
        server.get("/v1/no-such-route").await,
        server
            .post("/v1/uploads")
            .add_header("origin", "https://evil.test")
            .with_session(sessions::REP)
            .json(&json!({}))
            .await,
        server
            .post("/v1/uploads")
            .with_session(sessions::REP)
            .text("not json")
            .await,
    ];
    let statuses: Vec<u16> = responses.iter().map(|r| r.status_code().as_u16()).collect();
    assert_eq!(statuses, [200, 200, 200, 401, 404, 403, 400]);
    for res in &responses {
        let values: Vec<_> = res.iter_headers_by_name("x-content-type-options").collect();
        assert_eq!(values, ["nosniff"], "{}", res.status_code());
    }
}
