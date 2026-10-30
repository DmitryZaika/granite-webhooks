//! The 10DLC texting registration (`routes/phone/tendlc.rs`,
//! `routes/telnyx_webhooks/tendlc.rs`, `phone/tendlc*.rs`, the phone job's
//! 10DLC step): the company's Managers fill it in and mark it ready; only
//! superusers send it (the brand, then the campaign, once), read it again,
//! send it again after a rejection and assign the company's numbers;
//! Telnyx's signed notifications; the bell.
//!
//! Telnyx is a stateful stand-in below (`Fake`). Mark (101) is made a
//! Manager of Acme (100) and Sam (103) a superuser; Ada (102) is an admin
//! of Acme without the Manager position. Number `...0199` is one Telnyx
//! refuses to assign.

use crate::common;
use crate::phone_common;

use api::ai::Ai;
use api::app;
use api::auth::cookie::SessionData;
use api::phone::PhoneConfig;
use api::state::AppState;
use api::telnyx::Telnyx;
use api::webhooks_client::WebhooksClient;
use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum_test::{TestResponse, TestServer};
use common::{ALLOWED_ORIGIN, SECRET, WithSession, cookie_for, sessions};
use phone_common::{
    APP_URL, JOB_KEY, PUBLIC_API, Signer, TELNYX_KEY, add_number, enable_calling, hook_path, sql,
    webhook_base,
};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// One request the stand-in received: method, path and query, JSON body.
type Seen = (String, String, Value);

/// How the stand-in answers the next brand or campaign create.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Mode {
    /// Made and registered at once.
    #[default]
    Ok,
    /// Made, still registering (brands).
    Pending,
    /// Made, and TCR refused it (brands: `REGISTRATION_FAILED`).
    Failed,
    /// Refused outright (422): nothing made.
    Refuse,
    /// Made, but the answer is lost (500).
    LoseAnswer,
}

/// Telnyx's 10DLC API, kept in memory.
#[derive(Clone, Default)]
struct Fake {
    seen: Arc<Mutex<Vec<Seen>>>,
    brands: Arc<Mutex<Vec<Value>>>,
    campaigns: Arc<Mutex<Vec<Value>>>,
    assignments: Arc<Mutex<HashMap<String, Value>>>,
    next: Arc<AtomicUsize>,
    brand_mode: Arc<Mutex<Mode>>,
    campaign_mode: Arc<Mutex<Mode>>,
}

fn merge(base: &mut Value, patch: &Value) {
    match (base, patch) {
        (Value::Object(base), Value::Object(patch)) => {
            for (key, value) in patch {
                merge(base.entry(key.clone()).or_insert(Value::Null), value);
            }
        }
        (base, patch) => *base = patch.clone(),
    }
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "errors": [{ "code": "10005", "title": "Resource not found" }] })),
    )
        .into_response()
}

fn validation(field: &str, message: &str) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({ "detail": [{ "loc": ["body", field], "msg": message, "type": "value_error" }] })),
    )
        .into_response()
}

const FAILED_CAMPAIGN: [&str; 3] = ["TELNYX_FAILED", "MNO_REJECTED", "MNO_PROVISIONING_FAILED"];

impl Fake {
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    /// `METHOD path?query` of each request so far.
    fn calls(&self) -> Vec<String> {
        self.seen()
            .into_iter()
            .map(|(method, path, _)| format!("{method} {path}"))
            .collect()
    }

    /// The bodies sent with `method` to exactly `path` (query left out).
    fn bodies(&self, method: &str, path: &str) -> Vec<Value> {
        self.seen()
            .into_iter()
            .filter(|(m, uri, _)| m == method && uri.split('?').next() == Some(path))
            .map(|(_, _, body)| body)
            .collect()
    }

    fn count(&self, method: &str, path: &str) -> usize {
        self.bodies(method, path).len()
    }

    fn clear(&self) {
        self.seen.lock().unwrap().clear();
    }

    fn brand_mode(&self, mode: Mode) {
        *self.brand_mode.lock().unwrap() = mode;
    }

    fn campaign_mode(&self, mode: Mode) {
        *self.campaign_mode.lock().unwrap() = mode;
    }

    /// Changes the (only) brand the way Telnyx would on its side.
    fn set_brand(&self, patch: &Value) {
        merge(&mut self.brands.lock().unwrap()[0], patch);
    }

    /// Changes the newest campaign the way Telnyx would on its side.
    fn set_campaign(&self, patch: &Value) {
        let mut campaigns = self.campaigns.lock().unwrap();
        let last = campaigns.len() - 1;
        merge(&mut campaigns[last], patch);
    }

    fn set_assignment(&self, e164: &str, patch: &Value) {
        merge(
            self.assignments.lock().unwrap().get_mut(e164).unwrap(),
            patch,
        );
    }

    fn brand_record(&self, mode: Mode, body: &Value, id: &str) -> Value {
        let (status, identity, reasons) = match mode {
            Mode::Pending => ("REGISTRATION_PENDING", "SELF_DECLARED", Value::Null),
            Mode::Failed => (
                "REGISTRATION_FAILED",
                "UNVERIFIED",
                json!("Tax Id does not match the company name"),
            ),
            _ => ("OK", "VERIFIED", Value::Null),
        };
        let mut brand = body.clone();
        merge(
            &mut brand,
            &json!({ "brandId": id, "tcrBrandId": "BACME01", "status": status,
                     "identityStatus": identity, "failureReasons": reasons,
                     "brandRelationship": "BASIC_ACCOUNT", "createdAt": "2026-10-23T12:00:00" }),
        );
        brand
    }

    fn telnyx(&self, method: &str, uri: &str, body: &Value) -> Response {
        let ok = |record: Value| Json(record).into_response();
        let (path, query) = uri.split_once('?').unwrap_or((uri, ""));
        let query: HashMap<String, String> = url::form_urlencoded::parse(query.as_bytes())
            .into_owned()
            .collect();
        let parts: Vec<String> = path
            .trim_start_matches('/')
            .split('/')
            .map(|part| {
                percent_encoding::percent_decode_str(part)
                    .decode_utf8_lossy()
                    .to_string()
            })
            .collect();
        let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
        match (method, parts.as_slice()) {
            ("POST", ["10dlc", "brand"]) => {
                let mode = *self.brand_mode.lock().unwrap();
                if mode == Mode::Refuse {
                    return validation("ein", "Invalid EIN for +13175550100");
                }
                let n = self.next.fetch_add(1, Ordering::SeqCst) + 1;
                let brand = self.brand_record(mode, body, &format!("brand-{n}"));
                self.brands.lock().unwrap().push(brand.clone());
                if mode == Mode::LoseAnswer {
                    return (StatusCode::INTERNAL_SERVER_ERROR, "upstream timeout").into_response();
                }
                ok(brand)
            }
            ("GET", ["10dlc", "brand"]) => {
                let name = query.get("displayName").cloned().unwrap_or_default();
                let records: Vec<Value> = self
                    .brands
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|brand| brand["displayName"] == name.as_str())
                    .cloned()
                    .collect();
                ok(json!({ "records": records, "page": 1, "totalRecords": records.len() }))
            }
            ("GET", ["10dlc", "brand", id]) => {
                match self
                    .brands
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|brand| brand["brandId"] == *id)
                {
                    Some(brand) => ok(brand.clone()),
                    None => not_found(),
                }
            }
            ("PUT", ["10dlc", "brand", id]) => {
                let mode = *self.brand_mode.lock().unwrap();
                let mut brands = self.brands.lock().unwrap();
                let Some(brand) = brands.iter_mut().find(|brand| brand["brandId"] == *id) else {
                    return not_found();
                };
                let updated = self.brand_record(mode, body, id);
                merge(brand, &updated);
                ok(brand.clone())
            }
            ("GET", ["10dlc", "brand_feedback", id]) => {
                let failed = self.brands.lock().unwrap().iter().any(|brand| {
                    brand["brandId"] == *id && brand["status"] == "REGISTRATION_FAILED"
                });
                let category = if failed {
                    json!([{ "id": "TAX_ID", "displayName": "Tax Id",
                             "description": "Tax Id does not match with the company name or business type.",
                             "fields": ["ein", "companyName"] }])
                } else {
                    json!([])
                };
                ok(json!({ "brandId": id, "category": category }))
            }
            ("POST", ["10dlc", "campaignBuilder"]) => {
                let mode = *self.campaign_mode.lock().unwrap();
                if mode == Mode::Refuse {
                    return validation("sample1", "Sample message must name the brand");
                }
                let n = self.next.fetch_add(1, Ordering::SeqCst) + 1;
                let mut campaign = body.clone();
                merge(
                    &mut campaign,
                    &json!({ "campaignId": format!("camp-{n}"), "tcrCampaignId": format!("CACME0{n}"),
                             "campaignStatus": "TCR_PENDING", "status": "ACTIVE", "cspId": "TNX",
                             "mock": false, "failureReasons": null }),
                );
                self.campaigns.lock().unwrap().push(campaign.clone());
                if mode == Mode::LoseAnswer {
                    return (StatusCode::INTERNAL_SERVER_ERROR, "upstream timeout").into_response();
                }
                ok(campaign)
            }
            ("GET", ["10dlc", "campaign"]) => {
                let brand = query.get("brandId").cloned().unwrap_or_default();
                let records: Vec<Value> = self
                    .campaigns
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|campaign| campaign["brandId"] == brand.as_str())
                    .cloned()
                    .collect();
                ok(json!({ "records": records, "page": 1, "totalRecords": records.len() }))
            }
            ("GET", ["10dlc", "campaign", "usecase_cost"]) => ok(json!({
                "campaignUsecase": query.get("usecase").cloned().unwrap_or_default(),
                "upFrontCost": "30.00", "monthlyCost": "10.00",
                "description": "First three months charged up front" })),
            ("GET", ["10dlc", "campaign", id]) => {
                match self
                    .campaigns
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|campaign| campaign["campaignId"] == *id)
                {
                    Some(campaign) => ok(campaign.clone()),
                    None => not_found(),
                }
            }
            ("PUT", ["10dlc", "campaign", id]) => {
                let mut campaigns = self.campaigns.lock().unwrap();
                let Some(campaign) = campaigns
                    .iter_mut()
                    .find(|campaign| campaign["campaignId"] == *id)
                else {
                    return not_found();
                };
                merge(campaign, body);
                campaign["campaignStatus"] = json!("TCR_ACCEPTED");
                ok(campaign.clone())
            }
            ("POST", ["10dlc", "campaign", id, "appeal"]) => {
                let mut campaigns = self.campaigns.lock().unwrap();
                let Some(campaign) = campaigns
                    .iter_mut()
                    .find(|campaign| campaign["campaignId"] == *id)
                else {
                    return not_found();
                };
                let failed = campaign["campaignStatus"]
                    .as_str()
                    .is_some_and(|status| FAILED_CAMPAIGN.contains(&status));
                if !failed {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(
                            json!({ "errors": [{ "code": "10015", "title": "Bad Request",
                            "detail": "Campaign is not in an appealable status" }] }),
                        ),
                    )
                        .into_response();
                }
                campaign["campaignStatus"] = json!("TCR_ACCEPTED");
                ok(json!({ "appealed_at": "2026-10-23T12:30:00.000000+00:00" }))
            }
            ("POST", ["10dlc", "phone_number_campaigns"]) => {
                let number = body["phoneNumber"].as_str().unwrap_or_default().to_string();
                if number.ends_with("0199") {
                    return (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Json(json!({ "errors": [{ "code": "10015", "title": "Invalid",
                            "detail": format!("Phone number {number} is not on a messaging profile") }] })),
                    )
                        .into_response();
                }
                let assignment = json!({ "phoneNumber": number, "campaignId": body["campaignId"],
                    "telnyxCampaignId": body["campaignId"], "assignmentStatus": "PENDING_ASSIGNMENT",
                    "createdAt": "2026-10-23T12:00:00", "updatedAt": "2026-10-23T12:00:00" });
                self.assignments
                    .lock()
                    .unwrap()
                    .insert(number, assignment.clone());
                ok(assignment)
            }
            ("GET", ["10dlc", "phone_number_campaigns", number]) => {
                match self.assignments.lock().unwrap().get(*number) {
                    Some(assignment) => ok(assignment.clone()),
                    None => not_found(),
                }
            }
            _ => not_found(),
        }
    }

    async fn answer(&self, request: Request) -> Response {
        let (parts, body) = request.into_parts();
        let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let uri = parts
            .uri
            .path_and_query()
            .map_or_else(String::new, ToString::to_string);
        if let Some(telnyx) = uri.strip_prefix("/telnyx") {
            assert_eq!(
                parts.headers["authorization"],
                format!("Bearer {TELNYX_KEY}")
            );
            let method = parts.method.to_string();
            let path = percent_encoding::percent_decode_str(telnyx)
                .decode_utf8_lossy()
                .to_string();
            self.seen
                .lock()
                .unwrap()
                .push((method.clone(), path, json.clone()));
            return self.telnyx(&method, telnyx, &json);
        }
        StatusCode::NOT_FOUND.into_response()
    }
}

/// The API with Telnyx pointed at the stand-in; Telnyx's notifications
/// signed by `signer` (else unsigned ones pass).
async fn server(pool: &MySqlPool, signer: Option<&Signer>) -> (TestServer, Fake) {
    let fake = Fake::default();
    let handler = fake.clone();
    let router = Router::new().fallback(move |request: Request| {
        let handler = handler.clone();
        async move { handler.answer(request).await }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let base = format!("http://{address}");
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_telnyx(Telnyx::new(Some(TELNYX_KEY), &format!("{base}/telnyx")))
        .with_webhooks(WebhooksClient::new(
            Some(&format!("{base}/hooks")),
            Some("hooks-key"),
        ))
        .with_job_key(JOB_KEY)
        .with_app_url(APP_URL)
        .with_phone_config(PhoneConfig {
            public_api_url: Arc::from(PUBLIC_API),
            public_key: signer.map(|signer| Arc::from(signer.public_key.as_str())),
            allow_unsigned_webhooks: signer.is_none(),
            ..PhoneConfig::default()
        });
    (TestServer::new(app(state)), fake)
}

/// Mark (101) a Manager of Acme (100), Sam (103) a superuser; Acme's Telnyx
/// setup (texting through `profile-100`).
async fn setup(pool: &MySqlPool) {
    sql(
        pool,
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (101, 5, 100)",
    )
    .await;
    sql(pool, "UPDATE users SET is_superuser = 1 WHERE id = 103").await;
    enable_calling(pool, 100, "messaging_profile_id = 'profile-100'").await;
}

/// A complete form, as the page sends it.
fn form() -> Value {
    json!({
        "brand": {
            "entityType": "PRIVATE_PROFIT", "displayName": "Acme Stone",
            "companyName": "Acme Stone LLC", "ein": "123456789", "country": "us",
            "firstName": "Mark", "lastName": "Manager", "phone": "317-555-0100",
            "email": "office@acmestone.test", "website": "https://acmestone.test",
            "street": "100 Main St", "city": "Indianapolis", "state": "in",
            "postalCode": "46204", "vertical": "CONSTRUCTION"
        },
        "campaign": {
            "usecase": "MIXED", "subUsecases": ["CUSTOMER_CARE", "ACCOUNT_NOTIFICATION"],
            "description": "Acme Stone texts customers about their quotes, appointments and countertop projects.",
            "messageFlow": "Customers ask for a quote on our quote form at https://acmestone.test/quote and check an optional box to agree to texts from Acme Stone. Privacy policy: https://acmestone.test/privacy",
            "samples": [
                "Acme Stone: your countertop quote is ready. Reply STOP to opt out.",
                "Acme Stone: your install is Friday at 9 AM. Reply STOP to opt out.",
                "  "
            ],
            "helpMessage": "Acme Stone: for help call 317-555-0100 or email office@acmestone.test. Reply STOP to opt out.",
            "helpKeywords": "help, info",
            "optinMessage": "Acme Stone: you will get texts about your quotes and projects. Msg frequency varies. Msg & data rates may apply. Reply HELP for help, STOP to opt out.",
            "optinKeywords": "start",
            "optoutMessage": "Acme Stone: you are unsubscribed and will get no more texts. Reply START to subscribe again.",
            "optoutKeywords": "stop quit",
            "optinFormLink": "https://acmestone.test/quote",
            "privacyPolicyLink": "https://acmestone.test/privacy",
            "termsAndConditionsLink": "https://acmestone.test/terms",
            "termsAccepted": true
        }
    })
}

async fn save_form(server: &TestServer, session: &str, form: &Value) -> TestResponse {
    server
        .put("/v1/phone/admin/tendlc")
        .with_session(session)
        .json(form)
        .await
}

/// Mark saves `form` and marks it ready.
async fn ready(server: &TestServer, form: &Value) -> Value {
    save_form(server, sessions::MANAGER, form)
        .await
        .assert_status_ok();
    let response = server
        .post("/v1/phone/admin/tendlc/ready")
        .with_session(sessions::MANAGER)
        .await;
    response.assert_status_ok();
    response.json()
}

/// Sam's review of Acme's registration.
async fn review(server: &TestServer) -> Value {
    let response = server
        .get("/v1/superuser/companies/100/tendlc")
        .with_session(sessions::SUPER_ADMIN)
        .await;
    response.assert_status_ok();
    response.json()
}

async fn superuser_post(server: &TestServer, action: &str, body: &Value) -> TestResponse {
    server
        .post(&format!("/v1/superuser/companies/100/tendlc/{action}"))
        .with_session(sessions::SUPER_ADMIN)
        .json(body)
        .await
}

/// Bell notifications of `kind`: (user, message), oldest first.
async fn bell(pool: &MySqlPool, kind: &str) -> Vec<(i32, String)> {
    sqlx::query_as(
        "SELECT user_id, message FROM notifications WHERE notification_type = ? ORDER BY id",
    )
    .bind(kind)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// The registration's audit log: (actor, action).
async fn audit_actions(pool: &MySqlPool) -> Vec<(Option<i32>, String)> {
    sqlx::query_as(
        "SELECT actor_user_id, action FROM phone_audit_log
          WHERE company_id = 100 AND target LIKE 'tendlc:%' ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// Posts a Telnyx 10DLC notification for Acme.
async fn notify(server: &TestServer, signer: Option<&Signer>, event: &Value) -> TestResponse {
    let bytes = serde_json::to_vec(event).unwrap();
    let mut request = server
        .post(&hook_path(100, "10dlc"))
        .content_type("application/json")
        .bytes(Bytes::from(bytes.clone()));
    if let Some(signer) = signer {
        for (name, value) in signer.headers(&bytes) {
            request = request.add_header(name, value);
        }
    }
    request.await
}

fn event(id: &str, kind: &str, payload: Value) -> Value {
    json!({ "data": { "event_type": kind, "id": id, "occurred_at": "2026-10-23T12:00:00Z",
                      "payload": payload, "record_type": "event" },
            "meta": { "attempt": 1 } })
}

/// Fields of the registration's `problems`.
fn problem_fields(registration: &Value) -> Vec<String> {
    registration["problems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|problem| problem["field"].as_str().unwrap().to_string())
        .collect()
}

#[test_db::test(seed)]
async fn managers_fill_it_in_save_drafts_and_mark_it_ready(pool: MySqlPool) {
    setup(&pool).await;
    sql(
        &pool,
        "UPDATE company SET domain = 'acmestone.test' WHERE id = 100",
    )
    .await;
    add_number(&pool, 9701, 100, "+13175550150", "main", "Main line").await;
    let (server, fake) = server(&pool, None).await;
    // Before any save: a suggested first draft built from the company.
    let first: Value = server
        .get("/v1/phone/admin/tendlc")
        .with_session(sessions::MANAGER)
        .await
        .json();
    assert_eq!(first["saved"], false);
    assert_eq!(first["status"], "draft");
    assert_eq!(
        first["actions"],
        json!({ "edit": true, "markReady": false })
    );
    let brand = &first["draft"]["brand"];
    assert_eq!(brand["displayName"], "Acme Stone");
    assert_eq!(brand["email"], "manager@acme.test");
    assert_eq!(brand["phone"], "+13175550150");
    assert_eq!(brand["website"], "https://acmestone.test");
    assert_eq!(brand["vertical"], "CONSTRUCTION");
    let campaign = &first["draft"]["campaign"];
    assert_eq!(campaign["usecase"], "MIXED");
    for sample in campaign["samples"].as_array().unwrap() {
        let sample = sample.as_str().unwrap();
        assert!(sample.starts_with("Acme Stone:") && sample.ends_with("Reply STOP to opt out."));
    }
    assert!(
        campaign["helpMessage"]
            .as_str()
            .unwrap()
            .contains("317-555-0150")
    );
    // The business details are still to fill in.
    let missing = problem_fields(&first);
    for field in [
        "brand.ein",
        "brand.street",
        "brand.city",
        "brand.state",
        "brand.postalCode",
        "campaign.termsAccepted",
    ] {
        assert!(missing.contains(&field.to_string()), "{field}: {missing:?}");
    }

    // A draft saves however incomplete; marking it ready says what is missing.
    let mut partial = form();
    partial["brand"]["ein"] = json!("");
    partial["campaign"]["samples"][1] = json!("Your install is Friday. Reply STOP to opt out.");
    let saved = save_form(&server, sessions::MANAGER, &partial).await;
    saved.assert_status_ok();
    let saved: Value = saved.json();
    assert_eq!(saved["saved"], true);
    assert_eq!(saved["draftSavedByName"], "Mark Manager");
    assert_eq!(
        saved["problems"],
        json!([
            { "field": "brand.ein", "message": "Enter the tax id (EIN)." },
            { "field": "campaign.samples.1", "message": "Sample text 2 must name the business." },
        ])
    );
    // Saved as normalized: E.164, upper-case codes, keyword lists, no empty samples.
    let stored = &saved["draft"];
    assert_eq!(stored["brand"]["phone"], "+13175550100");
    assert_eq!(stored["brand"]["state"], "IN");
    assert_eq!(stored["brand"]["country"], "US");
    assert_eq!(stored["campaign"]["helpKeywords"], "HELP,INFO");
    assert_eq!(stored["campaign"]["optoutKeywords"], "STOP,QUIT");
    assert_eq!(stored["campaign"]["samples"].as_array().unwrap().len(), 2);
    let refused = server
        .post("/v1/phone/admin/tendlc/ready")
        .with_session(sessions::MANAGER)
        .await;
    refused.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        refused.json::<Value>()["error"],
        "The registration is not complete: Enter the tax id (EIN)."
    );
    // A field longer than Telnyx takes is refused at once.
    let mut long = form();
    long["brand"]["displayName"] = json!("A".repeat(101));
    save_form(&server, sessions::MANAGER, &long)
        .await
        .assert_status(StatusCode::BAD_REQUEST);

    // Complete, it can be marked ready: the superusers are told, once.
    let marked = ready(&server, &form()).await;
    assert_eq!(marked["status"], "ready");
    assert_eq!(marked["problems"], json!([]));
    assert_eq!(marked["readyByName"], "Mark Manager");
    // Every superuser (Sam, and those the migrations made), nobody else.
    let superusers: Vec<i32> = sqlx::query_scalar(
        "SELECT id FROM users WHERE is_superuser = 1 AND is_deleted = 0 ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(superusers.contains(&103));
    let told = bell(&pool, "tendlc_ready").await;
    let expected: Vec<(i32, String)> = superusers
        .iter()
        .map(|id| {
            (
                *id,
                "Acme Stone marked its texting registration (10DLC) ready to send to Telnyx."
                    .to_string(),
            )
        })
        .collect();
    assert_eq!(told, expected);
    server
        .post("/v1/phone/admin/tendlc/ready")
        .with_session(sessions::MANAGER)
        .await
        .assert_status_ok();
    assert_eq!(bell(&pool, "tendlc_ready").await.len(), superusers.len());
    // An edit that leaves it incomplete takes it back to draft.
    save_form(&server, sessions::MANAGER, &partial)
        .await
        .assert_status_ok();
    assert_eq!(review(&server).await["registration"]["status"], "draft");
    // Telnyx was never called: Managers never send.
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    assert_eq!(
        audit_actions(&pool).await,
        [
            (Some(101), "tendlc_saved".to_string()),
            (Some(101), "tendlc_saved".to_string()),
            (Some(101), "tendlc_ready".to_string()),
            (Some(101), "tendlc_saved".to_string()),
        ]
    );
}

#[test_db::test(seed)]
async fn only_the_companys_managers_fill_it_in_and_only_superusers_send(pool: MySqlPool) {
    setup(&pool).await;
    let (server, fake) = server(&pool, None).await;
    // Admins without the Manager position, reps and others: 403.
    for session in [
        sessions::ADMIN,
        sessions::REP,
        sessions::VIEWER,
        sessions::SUPER_ADMIN,
    ] {
        server
            .get("/v1/phone/admin/tendlc")
            .with_session(session)
            .await
            .assert_status(StatusCode::FORBIDDEN);
        save_form(&server, session, &form())
            .await
            .assert_status(StatusCode::FORBIDDEN);
        server
            .post("/v1/phone/admin/tendlc/ready")
            .with_session(session)
            .await
            .assert_status(StatusCode::FORBIDDEN);
    }
    server
        .get("/v1/phone/admin/tendlc")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    ready(&server, &form()).await;
    // Managers (and admins) can't send, read Telnyx or assign numbers.
    for session in [sessions::MANAGER, sessions::ADMIN] {
        for (method, path) in [
            ("GET", "/v1/superuser/tendlc"),
            ("GET", "/v1/superuser/companies/100/tendlc"),
            ("PUT", "/v1/superuser/companies/100/tendlc"),
            ("GET", "/v1/superuser/companies/100/tendlc/cost"),
            ("POST", "/v1/superuser/companies/100/tendlc/send"),
            ("POST", "/v1/superuser/companies/100/tendlc/resend"),
            ("POST", "/v1/superuser/companies/100/tendlc/refresh"),
            ("POST", "/v1/superuser/companies/100/tendlc/numbers"),
        ] {
            let request = match method {
                "GET" => server.get(path),
                "PUT" => server.put(path),
                _ => server.post(path),
            };
            request
                .with_session(session)
                .json(&form())
                .await
                .assert_status(StatusCode::FORBIDDEN);
        }
    }
    assert!(fake.calls().is_empty());
    // A Manager of another company sees and saves only hers.
    sql(
        &pool,
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (106, 5, 101)",
    )
    .await;
    let other: Value = server
        .get("/v1/phone/admin/tendlc")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .json();
    assert_eq!(other["saved"], false);
    assert_eq!(other["draft"]["brand"]["displayName"], "Other Granite Co");
    let mut theirs = form();
    theirs["brand"]["displayName"] = json!("Other Granite");
    save_form(&server, sessions::OTHER_COMPANY_REP, &theirs)
        .await
        .assert_status_ok();
    let ours = review(&server).await;
    assert_eq!(ours["registration"]["status"], "ready");
    assert_eq!(
        ours["registration"]["draft"]["brand"]["displayName"],
        "Acme Stone"
    );
    // Mark holds a position in Other Granite too, but is no super admin:
    // asking to act there still acts in his own company.
    let switched = cookie_for(SessionData {
        session_id: Some(sessions::MANAGER.into()),
        active_company_id: Some(101),
    });
    let mine: Value = server
        .get("/v1/phone/admin/tendlc")
        .add_header("cookie", switched)
        .await
        .json();
    assert_eq!(mine["draft"]["brand"]["displayName"], "Acme Stone");
    // The superusers' list shows both, the ready one first.
    let list: Value = server
        .get("/v1/superuser/tendlc")
        .with_session(sessions::SUPER_ADMIN)
        .await
        .json();
    let companies: Vec<(i64, &str)> = list["registrations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["companyId"].as_i64().unwrap(),
                row["status"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(companies, [(100, "ready"), (101, "draft")]);
    let ready_only: Value = server
        .get("/v1/superuser/tendlc?status=ready")
        .with_session(sessions::SUPER_ADMIN)
        .await
        .json();
    assert_eq!(ready_only["registrations"].as_array().unwrap().len(), 1);
    assert_eq!(ready_only["registrations"][0]["problemCount"], 0);
}

#[test_db::test(seed)]
async fn superusers_send_the_brand_then_the_campaign_once(pool: MySqlPool) {
    setup(&pool).await;
    let (server, fake) = server(&pool, None).await;
    ready(&server, &form()).await;
    let before = review(&server).await;
    assert_eq!(
        before["actions"],
        json!({ "edit": true, "send": true, "resend": false, "refresh": false,
                "assignNumbers": false })
    );
    assert_eq!(before["telnyxSetUp"], true);
    assert_eq!(before["messagingProfileSet"], true);
    // Telnyx's price, before sending.
    let cost: Value = server
        .get("/v1/superuser/companies/100/tendlc/cost")
        .with_session(sessions::SUPER_ADMIN)
        .await
        .json();
    assert_eq!(
        cost,
        json!({ "usecase": "MIXED", "upFrontCost": "30.00", "monthlyCost": "10.00",
                "description": "First three months charged up front" })
    );
    fake.clear();

    let sent = superuser_post(&server, "send", &json!({})).await;
    sent.assert_status_ok();
    let sent: Value = sent.json();
    assert_eq!(sent["registration"]["status"], "campaign_pending");
    assert_eq!(sent["registration"]["brandStatus"], "OK");
    assert_eq!(sent["registration"]["campaignStatus"], "TCR_PENDING");
    assert_eq!(sent["registration"]["campaignLocked"], true);
    assert_eq!(sent["sentByName"], "Sam Super");
    assert_eq!(
        sent["telnyx"],
        json!({ "brandId": "brand-1", "tcrBrandId": "BACME01", "campaignId": "camp-2",
                "tcrCampaignId": "CACME02" })
    );
    let registration_id: i32 =
        sqlx::query_scalar("SELECT id FROM tendlc_registrations WHERE company_id = 100")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        fake.calls(),
        [
            "POST /10dlc/brand".to_string(),
            "GET /10dlc/campaign?brandId=brand-1&recordsPerPage=100".to_string(),
            "POST /10dlc/campaignBuilder".to_string(),
        ]
    );
    let hook = format!("{}/10dlc", webhook_base(100));
    assert_eq!(
        fake.bodies("POST", "/10dlc/brand"),
        [json!({
            "entityType": "PRIVATE_PROFIT", "displayName": "Acme Stone", "companyName": "Acme Stone LLC",
            "ein": "12-3456789", "phone": "+13175550100", "street": "100 Main St",
            "city": "Indianapolis", "state": "IN", "postalCode": "46204", "country": "US",
            "email": "office@acmestone.test", "website": "https://acmestone.test",
            "vertical": "CONSTRUCTION", "firstName": "Mark", "lastName": "Manager",
            "webhookURL": hook,
        })]
    );
    let campaign = &fake.bodies("POST", "/10dlc/campaignBuilder")[0];
    assert_eq!(campaign["brandId"], "brand-1");
    assert_eq!(
        campaign["referenceId"],
        format!("granite-c100-r{registration_id}-1")
    );
    assert_eq!(campaign["usecase"], "MIXED");
    assert_eq!(
        campaign["subUsecases"],
        json!(["CUSTOMER_CARE", "ACCOUNT_NOTIFICATION"])
    );
    assert_eq!(
        campaign["sample1"],
        "Acme Stone: your countertop quote is ready. Reply STOP to opt out."
    );
    assert_eq!(campaign.get("sample3"), None);
    assert_eq!(campaign["optoutKeywords"], "STOP,QUIT");
    assert_eq!(campaign["termsAndConditions"], true);
    assert_eq!(campaign["webhookURL"], hook);

    // Sending again makes nothing: no second brand, no second campaign.
    superuser_post(&server, "send", &json!({}))
        .await
        .assert_status(StatusCode::CONFLICT);
    assert_eq!(fake.count("POST", "/10dlc/brand"), 1);
    assert_eq!(fake.count("POST", "/10dlc/campaignBuilder"), 1);
    // The Manager can no longer change it.
    save_form(&server, sessions::MANAGER, &form())
        .await
        .assert_status(StatusCode::CONFLICT);
    assert_eq!(
        audit_actions(&pool).await[2..],
        [
            (Some(103), "tendlc_sent".to_string()),
            (None, "tendlc_campaign_created".to_string()),
        ]
    );
}

#[test_db::test(seed)]
async fn a_send_checks_the_form_the_setup_and_telnyxs_refusal(pool: MySqlPool) {
    setup(&pool).await;
    let (server, fake) = server(&pool, None).await;
    // Nothing saved yet.
    superuser_post(&server, "send", &json!({}))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    // Incomplete.
    let mut partial = form();
    partial["campaign"]["termsAccepted"] = json!(false);
    save_form(&server, sessions::MANAGER, &partial)
        .await
        .assert_status_ok();
    let incomplete = superuser_post(&server, "send", &json!({})).await;
    incomplete.assert_status(StatusCode::BAD_REQUEST);
    assert!(
        incomplete.json::<Value>()["error"]
            .as_str()
            .unwrap()
            .contains("Accept the carriers' terms")
    );
    // Telnyx refuses the brand: nothing made, its reason shown masked.
    ready(&server, &form()).await;
    fake.brand_mode(Mode::Refuse);
    let refused = superuser_post(&server, "send", &json!({})).await;
    refused.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        refused.json::<Value>()["error"],
        "Telnyx did not accept the brand: Invalid EIN for [number] (ein)"
    );
    let after = review(&server).await;
    assert_eq!(after["registration"]["status"], "ready");
    assert_eq!(after["telnyx"]["brandId"], Value::Null);
    assert!(
        after["registration"]["lastError"]
            .as_str()
            .unwrap()
            .contains("[number]")
    );
    // Without the company's Telnyx setup there is nowhere for Telnyx to report.
    sql(
        &pool,
        "DELETE FROM telnyx_company_config WHERE company_id = 100",
    )
    .await;
    superuser_post(&server, "send", &json!({}))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(fake.count("POST", "/10dlc/brand"), 1);
}

#[test_db::test(seed)]
async fn a_brand_whose_answer_never_came_is_found_not_made_twice(pool: MySqlPool) {
    setup(&pool).await;
    let (server, fake) = server(&pool, None).await;
    ready(&server, &form()).await;
    fake.brand_mode(Mode::LoseAnswer);
    superuser_post(&server, "send", &json!({}))
        .await
        .assert_status(StatusCode::BAD_GATEWAY);
    assert_eq!(review(&server).await["telnyx"]["brandId"], Value::Null);
    fake.brand_mode(Mode::Ok);
    fake.clear();
    let sent = superuser_post(&server, "send", &json!({})).await;
    sent.assert_status_ok();
    assert_eq!(
        sent.json::<Value>()["registration"]["status"],
        "campaign_pending"
    );
    assert_eq!(
        fake.calls(),
        [
            "GET /10dlc/brand?displayName=Acme+Stone&recordsPerPage=100&sort=-createdAt"
                .to_string(),
            "GET /10dlc/campaign?brandId=brand-1&recordsPerPage=100".to_string(),
            "POST /10dlc/campaignBuilder".to_string(),
        ]
    );
    assert_eq!(fake.brands.lock().unwrap().len(), 1);
}

#[test_db::test(seed)]
async fn a_pending_brand_gets_its_campaign_from_the_job_and_approval_is_told(pool: MySqlPool) {
    setup(&pool).await;
    let (server, fake) = server(&pool, None).await;
    ready(&server, &form()).await;
    fake.brand_mode(Mode::Pending);
    let sent: Value = superuser_post(&server, "send", &json!({})).await.json();
    assert_eq!(sent["registration"]["status"], "sent");
    assert_eq!(sent["registration"]["brandStatus"], "REGISTRATION_PENDING");
    assert_eq!(fake.count("POST", "/10dlc/campaignBuilder"), 0);
    // TCR registers the brand: the job reads it again and creates the campaign.
    fake.set_brand(&json!({ "status": "OK", "identityStatus": "VERIFIED" }));
    let run: Value = server
        .post("/v1/jobs/phone")
        .add_header("authorization", JOB_KEY)
        .await
        .json();
    assert_eq!(run["tendlc_refreshed"], 1);
    assert_eq!(fake.count("POST", "/10dlc/campaignBuilder"), 1);
    assert_eq!(
        review(&server).await["registration"]["status"],
        "campaign_pending"
    );
    // A run right after leaves it alone (read again hourly while in review).
    let again: Value = server
        .post("/v1/jobs/phone")
        .add_header("authorization", JOB_KEY)
        .await
        .json();
    assert_eq!(again.get("tendlc_refreshed"), None);
    // The carriers approve it: a refresh moves it on and the company is told once.
    fake.set_campaign(&json!({ "campaignStatus": "MNO_PROVISIONED" }));
    let refreshed: Value = superuser_post(&server, "refresh", &json!({})).await.json();
    assert_eq!(refreshed["registration"]["status"], "approved");
    assert!(refreshed["registration"]["approvedAt"].is_string());
    assert_eq!(refreshed["actions"]["assignNumbers"], true);
    superuser_post(&server, "refresh", &json!({}))
        .await
        .assert_status_ok();
    let told = bell(&pool, "tendlc_status").await;
    let people: Vec<i32> = told.iter().map(|(user, _)| *user).collect();
    assert!(people.contains(&101) && people.contains(&102), "{people:?}");
    assert!(
        told.iter().all(
            |(_, message)| message.starts_with("Your texting registration (10DLC) is approved")
        )
    );
    assert_eq!(told.len(), people.len());
}

#[test_db::test(seed)]
async fn telnyxs_signed_notifications_move_it_and_keep_the_reasons(pool: MySqlPool) {
    setup(&pool).await;
    let signer = Signer::new();
    let (server, fake) = server(&pool, Some(&signer)).await;
    ready(&server, &form()).await;
    superuser_post(&server, "send", &json!({}))
        .await
        .assert_status_ok();
    let rejection = event(
        "evt-1",
        "10dlc.campaign.update",
        json!({ "campaignId": "camp-2", "status": "REJECTED", "type": "MNO_REVIEW",
                "description": "Rejected by T-Mobile: opt-out line +13175550100 is a phone number" }),
    );
    // Unsigned, or signed by someone else: refused.
    notify(&server, None, &rejection)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    notify(&server, Some(&Signer::new()), &rejection)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    assert_eq!(
        review(&server).await["registration"]["status"],
        "campaign_pending"
    );
    // Signed: the campaign is read again and the reason kept, numbers masked.
    fake.set_campaign(&json!({ "campaignStatus": "MNO_REJECTED" }));
    notify(&server, Some(&signer), &rejection)
        .await
        .assert_status_ok();
    let rejected = review(&server).await;
    assert_eq!(rejected["registration"]["status"], "rejected");
    assert_eq!(
        rejected["registration"]["campaignFeedback"],
        json!([{ "title": "Carrier review",
                 "description": "Rejected by T-Mobile: opt-out line [number] is a phone number",
                 "fields": [] }])
    );
    assert_eq!(rejected["registration"]["actions"]["markReady"], true);
    let told = bell(&pool, "tendlc_status").await;
    assert!(!told.is_empty());
    assert!(told[0].1.contains("needs changes"));
    // The same notification again changes nothing.
    notify(&server, Some(&signer), &rejection)
        .await
        .assert_status_ok();
    assert_eq!(bell(&pool, "tendlc_status").await.len(), told.len());
    // About another campaign: ignored.
    let other = event(
        "evt-2",
        "10dlc.campaign.update",
        json!({ "campaignId": "camp-elsewhere", "status": "ACCEPTED", "type": "VERIFIED" }),
    );
    notify(&server, Some(&signer), &other)
        .await
        .assert_status_ok();
    assert_eq!(review(&server).await["registration"]["status"], "rejected");
    // An unknown token is 404.
    server
        .post("/v1/webhooks/telnyx/tok-unknown/10dlc")
        .json(&other)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn a_rejected_campaign_is_fixed_and_sent_again(pool: MySqlPool) {
    setup(&pool).await;
    let (server, fake) = server(&pool, None).await;
    ready(&server, &form()).await;
    superuser_post(&server, "send", &json!({}))
        .await
        .assert_status_ok();
    fake.set_campaign(&json!({ "campaignStatus": "TELNYX_FAILED",
                               "failureReasons": "Sample messages do not match the use case" }));
    let refreshed: Value = superuser_post(&server, "refresh", &json!({})).await.json();
    assert_eq!(refreshed["registration"]["status"], "rejected");
    assert_eq!(
        refreshed["registration"]["campaignFeedback"][0]["description"],
        "Sample messages do not match the use case"
    );
    // Nothing changed and no appeal: nothing to send again.
    superuser_post(&server, "resend", &json!({}))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    // Telnyx has the campaign: the use case can't change, the samples can.
    let mut changed = form();
    changed["campaign"]["usecase"] = json!("CUSTOMER_CARE");
    let locked = save_form(&server, sessions::MANAGER, &changed).await;
    locked.assert_status(StatusCode::BAD_REQUEST);
    let mut fixed = form();
    fixed["campaign"]["samples"][1] =
        json!("Acme Stone: your account balance is paid in full. Reply STOP to opt out.");
    ready(&server, &fixed).await;
    // Marked ready again after the fix: Sam is told a second time.
    let to_sam = bell(&pool, "tendlc_ready")
        .await
        .into_iter()
        .filter(|(user, _)| *user == 103)
        .count();
    assert_eq!(to_sam, 2);
    fake.clear();
    let resent = superuser_post(
        &server,
        "resend",
        &json!({ "appealReason": "We rewrote the samples to match the account updates we send." }),
    )
    .await;
    resent.assert_status_ok();
    let resent: Value = resent.json();
    assert_eq!(resent["registration"]["status"], "campaign_pending");
    assert_eq!(resent["registration"]["campaignFeedback"], json!([]));
    assert!(resent["resentAt"].is_string());
    // The appeal first (Telnyx takes one only while refused), then the samples.
    assert_eq!(
        fake.calls(),
        [
            "POST /10dlc/campaign/camp-2/appeal".to_string(),
            "PUT /10dlc/campaign/camp-2".to_string(),
        ]
    );
    assert_eq!(
        fake.bodies("POST", "/10dlc/campaign/camp-2/appeal"),
        [
            json!({ "appeal_reason": "We rewrote the samples to match the account updates we send." })
        ]
    );
    let update = &fake.bodies("PUT", "/10dlc/campaign/camp-2")[0];
    assert_eq!(
        update["sample2"],
        "Acme Stone: your account balance is paid in full. Reply STOP to opt out."
    );
    assert_eq!(update.get("usecase"), None);
    // It is with Telnyx again: nothing more to send.
    superuser_post(&server, "resend", &json!({}))
        .await
        .assert_status(StatusCode::CONFLICT);
    let actions: Vec<String> = audit_actions(&pool)
        .await
        .into_iter()
        .map(|(_, action)| action)
        .collect();
    assert_eq!(actions.last().map(String::as_str), Some("tendlc_resent"));
}

#[test_db::test(seed)]
async fn a_refused_brand_is_fixed_and_its_campaign_follows(pool: MySqlPool) {
    setup(&pool).await;
    let (server, fake) = server(&pool, None).await;
    ready(&server, &form()).await;
    fake.brand_mode(Mode::Failed);
    let sent: Value = superuser_post(&server, "send", &json!({})).await.json();
    assert_eq!(sent["registration"]["status"], "rejected");
    assert_eq!(sent["registration"]["brandStatus"], "REGISTRATION_FAILED");
    assert_eq!(
        sent["registration"]["brandFeedback"],
        json!([
            { "title": "Tax Id",
              "description": "Tax Id does not match with the company name or business type.",
              "fields": ["ein", "companyName"] },
            { "title": "Brand registration",
              "description": "Tax Id does not match the company name", "fields": [] },
        ])
    );
    assert_eq!(fake.count("POST", "/10dlc/campaignBuilder"), 0);
    // The Manager fixes the legal name; the superuser sends it again.
    let mut fixed = form();
    fixed["brand"]["companyName"] = json!("Acme Stone Holdings LLC");
    ready(&server, &fixed).await;
    fake.brand_mode(Mode::Ok);
    fake.clear();
    let resent: Value = superuser_post(&server, "resend", &json!({})).await.json();
    assert_eq!(resent["registration"]["status"], "campaign_pending");
    assert_eq!(resent["registration"]["brandFeedback"], json!([]));
    assert_eq!(
        fake.calls(),
        [
            "PUT /10dlc/brand/brand-1".to_string(),
            "GET /10dlc/campaign?brandId=brand-1&recordsPerPage=100".to_string(),
            "POST /10dlc/campaignBuilder".to_string(),
        ]
    );
    assert_eq!(
        fake.bodies("PUT", "/10dlc/brand/brand-1")[0]["companyName"],
        "Acme Stone Holdings LLC"
    );
    assert_eq!(fake.brands.lock().unwrap().len(), 1);
}

#[test_db::test(seed)]
async fn a_campaign_tcr_refused_is_replaced_by_a_new_one(pool: MySqlPool) {
    setup(&pool).await;
    let (server, fake) = server(&pool, None).await;
    ready(&server, &form()).await;
    // Telnyx refuses to create the campaign: rejected with its reason.
    fake.campaign_mode(Mode::Refuse);
    let sent: Value = superuser_post(&server, "send", &json!({})).await.json();
    assert_eq!(sent["registration"]["status"], "rejected");
    assert_eq!(sent["registration"]["campaignStatus"], "REFUSED");
    assert_eq!(
        sent["registration"]["campaignFeedback"][0]["description"],
        "Sample message must name the brand (sample1)"
    );
    // Sent again as it is: the campaign is created.
    fake.campaign_mode(Mode::Ok);
    ready(&server, &form()).await;
    let resent: Value = superuser_post(&server, "resend", &json!({})).await.json();
    assert_eq!(resent["registration"]["status"], "campaign_pending");
    let registration_id: i32 =
        sqlx::query_scalar("SELECT id FROM tendlc_registrations WHERE company_id = 100")
            .fetch_one(&pool)
            .await
            .unwrap();
    // TCR then refuses that campaign for good: a new one, with a new reference.
    fake.set_campaign(&json!({ "campaignStatus": "TCR_FAILED" }));
    superuser_post(&server, "refresh", &json!({}))
        .await
        .assert_status_ok();
    ready(&server, &form()).await;
    let replaced: Value = superuser_post(&server, "resend", &json!({})).await.json();
    assert_eq!(replaced["registration"]["status"], "campaign_pending");
    let references: Vec<Value> = fake
        .bodies("POST", "/10dlc/campaignBuilder")
        .iter()
        .map(|body| body["referenceId"].clone())
        .collect();
    assert_eq!(
        references,
        [
            json!(format!("granite-c100-r{registration_id}-1")),
            json!(format!("granite-c100-r{registration_id}-1")),
            json!(format!("granite-c100-r{registration_id}-2")),
        ]
    );
    assert_eq!(replaced["telnyx"]["campaignId"], "camp-3");
}

#[test_db::test(seed)]
async fn numbers_are_assigned_to_the_approved_campaign(pool: MySqlPool) {
    setup(&pool).await;
    let signer = Signer::new();
    let (server, fake) = server(&pool, Some(&signer)).await;
    add_number(&pool, 9701, 100, "+13175550151", "direct", "Rita").await;
    add_number(&pool, 9702, 100, "+13175550152", "main", "Main line").await;
    add_number(&pool, 9703, 100, "+13175550199", "direct", "Old line").await;
    ready(&server, &form()).await;
    superuser_post(&server, "send", &json!({}))
        .await
        .assert_status_ok();
    // Not before the carriers approve the campaign.
    superuser_post(&server, "numbers", &json!({}))
        .await
        .assert_status(StatusCode::CONFLICT);
    fake.set_campaign(&json!({ "campaignStatus": "MNO_PROVISIONED" }));
    superuser_post(&server, "refresh", &json!({}))
        .await
        .assert_status_ok();
    let assigned = superuser_post(&server, "numbers", &json!({})).await;
    assigned.assert_status_ok();
    let assigned: Value = assigned.json();
    assert_eq!(
        (
            &assigned["assigned"],
            &assigned["failed"],
            &assigned["skipped"]
        ),
        (&json!(2), &json!(1), &json!(0))
    );
    let numbers: Vec<(String, Value, Value)> = assigned["review"]["numbers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|number| {
            (
                number["e164"].as_str().unwrap().to_string(),
                number["status"].clone(),
                number["error"].clone(),
            )
        })
        .collect();
    assert_eq!(
        numbers,
        [
            (
                "+13175550151".to_string(),
                json!("PENDING_ASSIGNMENT"),
                Value::Null
            ),
            (
                "+13175550152".to_string(),
                json!("PENDING_ASSIGNMENT"),
                Value::Null
            ),
            (
                "+13175550199".to_string(),
                json!("FAILED_ASSIGNMENT"),
                json!("Phone number [number] is not on a messaging profile")
            ),
        ]
    );
    assert_eq!(
        fake.bodies("POST", "/10dlc/phone_number_campaigns")[0],
        json!({ "phoneNumber": "+13175550151", "campaignId": "camp-2" })
    );
    // Telnyx confirms one by notification, the other is read again by the job.
    notify(
        &server,
        Some(&signer),
        &event(
            "evt-n1",
            "10dlc.phone_number.update",
            json!({ "campaignId": "camp-2", "phoneNumber": "+13175550151", "status": "success",
                    "type": "ASSIGNMENT", "reasons": [] }),
        ),
    )
    .await
    .assert_status_ok();
    fake.set_assignment("+13175550152", &json!({ "assignmentStatus": "ASSIGNED" }));
    sql(&pool, "UPDATE tendlc_numbers SET refreshed_at = NULL").await;
    sql(
        &pool,
        "UPDATE tendlc_registrations SET refreshed_at = UTC_TIMESTAMP()",
    )
    .await;
    let run: Value = server
        .post("/v1/jobs/phone")
        .add_header("authorization", JOB_KEY)
        .await
        .json();
    assert_eq!(run["tendlc_numbers_refreshed"], 1);
    let statuses: Vec<(String, String)> =
        sqlx::query_as("SELECT e164, status FROM tendlc_numbers ORDER BY e164")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        statuses,
        [
            ("+13175550151".to_string(), "ASSIGNED".to_string()),
            ("+13175550152".to_string(), "ASSIGNED".to_string()),
            ("+13175550199".to_string(), "FAILED_ASSIGNMENT".to_string()),
        ]
    );
    // Assigning again retries only the one that failed.
    fake.clear();
    let again: Value = superuser_post(&server, "numbers", &json!({})).await.json();
    assert_eq!(
        (&again["assigned"], &again["failed"], &again["skipped"]),
        (&json!(0), &json!(1), &json!(2))
    );
    assert_eq!(fake.count("POST", "/10dlc/phone_number_campaigns"), 1);
    let assignments: Vec<(Option<i32>, String)> = audit_actions(&pool)
        .await
        .into_iter()
        .filter(|(_, action)| action == "tendlc_numbers_assigned")
        .collect();
    assert_eq!(
        assignments,
        [
            (Some(103), "tendlc_numbers_assigned".to_string()),
            (Some(103), "tendlc_numbers_assigned".to_string()),
        ]
    );
}

#[test_db::test(seed)]
async fn the_bell_lists_texting_registration_notices(pool: MySqlPool) {
    setup(&pool).await;
    let (server, fake) = server(&pool, None).await;
    ready(&server, &form()).await;
    superuser_post(&server, "send", &json!({}))
        .await
        .assert_status_ok();
    fake.set_campaign(&json!({ "campaignStatus": "MNO_PROVISIONED" }));
    superuser_post(&server, "refresh", &json!({}))
        .await
        .assert_status_ok();
    let item = |bell: &Value, kind: &str| -> Value {
        bell["notifications"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["notification_type"] == kind)
            .cloned()
            .unwrap_or(Value::Null)
    };
    let superuser: Value = server
        .get("/v1/notifications")
        .with_session(sessions::SUPER_ADMIN)
        .await
        .json();
    let ready_item = item(&superuser, "tendlc_ready");
    assert_eq!(ready_item["title"], "Texting registration");
    assert_eq!(
        ready_item["href"],
        "/admin/superuser-dashboard/companies/tendlc"
    );
    assert_eq!(ready_item["type_title"], "Texting Registration");
    assert_eq!(ready_item["actor_name"], "Mark Manager");
    let admin: Value = server
        .get("/v1/notifications")
        .with_session(sessions::ADMIN)
        .await
        .json();
    let status_item = item(&admin, "tendlc_status");
    assert_eq!(status_item["href"], "/admin/phone");
    assert_eq!(status_item["actor_name"], "Telnyx 10DLC");
    assert_eq!(status_item.get("customer_name"), None);
}

// ----------------------------------------------------------- the AI check

/// The AI check's stand-ins: Acme's website (`/site/...`), OpenAI
/// (`/openai/chat`, answering from a queue) and the email service
/// (`/hooks/email/send`).
#[derive(Clone, Default)]
struct CheckFake {
    pages: Arc<Mutex<HashMap<String, String>>>,
    answers: Arc<Mutex<Vec<Value>>>,
    requests: Arc<Mutex<Vec<Value>>>,
    emails: Arc<Mutex<Vec<Value>>>,
}

impl CheckFake {
    fn page(&self, path: &str, html: &str) {
        self.pages
            .lock()
            .unwrap()
            .insert(path.to_string(), html.to_string());
    }

    /// The AI's next answer.
    fn answer_with(&self, answer: Value) {
        self.answers.lock().unwrap().push(answer);
    }

    fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }

    /// The emails sent: (to, subject, body).
    fn emails(&self) -> Vec<(String, String, String)> {
        self.emails
            .lock()
            .unwrap()
            .iter()
            .map(|email| {
                let text = |value: &Value| value.as_str().unwrap_or_default().to_string();
                (
                    text(&email["to"][0]),
                    text(&email["subject"]),
                    text(&email["body"]),
                )
            })
            .collect()
    }

    async fn answer(&self, request: Request) -> Response {
        let (parts, body) = request.into_parts();
        let bytes = axum::body::to_bytes(body, 8 << 20).await.unwrap();
        let path = parts.uri.path().to_string();
        if let Some(page) = path.strip_prefix("/site") {
            let html = self.pages.lock().unwrap().get(page).cloned();
            return match html {
                Some(html) => {
                    ([("content-type", "text/html; charset=utf-8")], html).into_response()
                }
                None => StatusCode::NOT_FOUND.into_response(),
            };
        }
        let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if path == "/openai/chat" {
            self.requests.lock().unwrap().push(json);
            let answer = {
                let mut answers = self.answers.lock().unwrap();
                if answers.is_empty() {
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                answers.remove(0)
            };
            return Json(json!({
                "model": "stand-in",
                "choices": [{ "message": { "content": answer.to_string() }, "finish_reason": "stop" }],
                "usage": { "prompt_tokens": 1000, "completion_tokens": 100 },
            }))
            .into_response();
        }
        if path == "/hooks/email/send" {
            self.emails.lock().unwrap().push(json);
            return Json(json!({ "message_id": "m-1" })).into_response();
        }
        StatusCode::NOT_FOUND.into_response()
    }
}

/// The API with an AI, email and Acme's website on the stand-ins (the AI
/// check may read their loopback address); the stand-ins' base URL.
async fn check_server(pool: &MySqlPool) -> (TestServer, CheckFake, String) {
    let fake = CheckFake::default();
    let handler = fake.clone();
    let router = Router::new().fallback(move |request: Request| {
        let handler = handler.clone();
        async move { handler.answer(request).await }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let base = format!("http://{address}");
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_ai(Ai::new(Some("ai-key"), &format!("{base}/openai/chat")))
        .with_webhooks(WebhooksClient::new(
            Some(&format!("{base}/hooks")),
            Some("hooks-key"),
        ))
        .with_job_key(JOB_KEY)
        .with_app_url(APP_URL)
        .with_phone_config(PhoneConfig {
            allow_private_pages: true,
            ..PhoneConfig::default()
        });
    (TestServer::new(app(state)), fake, base)
}

const QUOTE_PAGE: &str = r#"<!doctype html><html><head><title>Free quote | Acme Stone</title>
<script>track("visit")</script></head><body><div class="wrap"><h1>Get a free countertop quote</h1>
<form action="/quote" method="post" class="quote-form">
  <label for="name">Name</label><input id="name" name="name" required>
  <label for="phone">Mobile phone</label><input type="tel" id="phone" name="phone" required>
  <input type="checkbox" id="sms" name="sms">
  <label for="sms">I agree to receive texts from Acme Stone about my quote and project. Message
  frequency varies. Msg &amp; data rates may apply. Reply HELP for help, STOP to opt out.</label>
  <a href="/site/privacy">Privacy Policy</a> <a href="/site/terms">Terms</a>
  <button type="submit">Send</button>
</form></div></body></html>"#;
const PRIVACY_PAGE: &str = "<html><body><h1>Acme Stone privacy policy</h1><p>We collect your \
    name and phone number when you ask for a quote.</p><p>We may share your information with \
    marketing partners.</p></body></html>";
const FIXED_PRIVACY_PAGE: &str = "<html><body><h1>Acme Stone privacy policy</h1><p>We collect \
    your name and phone number when you ask for a quote.</p><p>No mobile information will be \
    shared with third parties or affiliates for marketing or promotional purposes.</p></body></html>";
const TERMS_PAGE: &str = "<html><body><h1>Acme Stone terms</h1><h2>Text messages</h2><p>Acme \
    Stone texts customers who agree on our quote form. Message frequency varies. Msg &amp; data \
    rates may apply. Reply STOP to opt out, HELP for help. Carriers are not liable for delayed or \
    undelivered messages.</p></body></html>";
const HOME_PAGE: &str = "<html><body><h1>Acme Stone</h1><p>Granite and quartz countertops in \
    Indianapolis since 2004.</p><footer>100 Main St, Indianapolis, IN 46204 · (317) 555-0100 · \
    office@acmestone.test</footer></body></html>";

/// An answer of the AI: every check passes but those of `changed` (id,
/// answer without its id).
fn ai_answer(summary: &str, changed: &[(&str, Value)]) -> Value {
    let checks: Vec<Value> = api::phone::tendlc_check::CHECKS
        .iter()
        .map(|check| {
            changed.iter().find(|(id, _)| *id == check.id).map_or_else(
                || {
                    json!({ "id": check.id, "status": "pass", "evidence": "Acme Stone",
                                "note": "", "fixes": [] })
                },
                |(_, given)| {
                    let mut given = given.clone();
                    given["id"] = json!(check.id);
                    given
                },
            )
        })
        .collect();
    json!({ "summary": summary, "checks": checks })
}

async fn run_checks(server: &TestServer) -> Value {
    let run = server
        .post("/v1/jobs/tendlc-checks")
        .add_header("authorization", JOB_KEY)
        .await;
    run.assert_status_ok();
    run.json()
}

async fn email_of(pool: &MySqlPool, user: i32) -> String {
    sqlx::query_scalar("SELECT email FROM users WHERE id = ?")
        .bind(user)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[test_db::test(seed)]
async fn the_ai_checks_the_form_and_pages_before_the_superusers_get_it(pool: MySqlPool) {
    setup(&pool).await;
    let (server, fake, base) = check_server(&pool).await;
    fake.page("/quote", QUOTE_PAGE);
    fake.page("/privacy", PRIVACY_PAGE);
    fake.page("/terms", TERMS_PAGE);
    fake.page("/", HOME_PAGE);
    let mut form = form();
    form["brand"]["website"] = json!(format!("{base}/site/"));
    form["campaign"]["optinFormLink"] = json!(format!("{base}/site/quote"));
    form["campaign"]["privacyPolicyLink"] = json!(format!("{base}/site/privacy"));
    form["campaign"]["termsAndConditionsLink"] = json!(format!("{base}/site/terms"));
    form["campaign"]["messageFlow"] = json!(format!(
        "Customers ask for a quote on our quote form at {base}/site/quote and agree to texts from \
         Acme Stone. Privacy policy: {base}/site/privacy"
    ));
    let mark = email_of(&pool, 101).await;
    let sam = email_of(&pool, 103).await;
    save_form(&server, sessions::MANAGER, &form)
        .await
        .assert_status_ok();

    // Marking it ready asks for the check: still a draft, nobody told yet.
    let marked = ready(&server, &form).await;
    assert_eq!(marked["status"], "draft");
    assert_eq!(marked["aiCheck"]["status"], "pending");
    assert_eq!(marked["actions"]["markReady"], false);
    assert!(bell(&pool, "tendlc_ready").await.is_empty());

    // The AI's first answer passes the website on a quote that is on no
    // page: it is sent back, and the second answer asks for two changes.
    let flow = format!(
        "Customers ask for a quote on our quote form at {base}/site/quote and check an optional \
         box, not checked in advance, to agree to texts from Acme Stone: \"I agree to receive \
         texts from Acme Stone about my quote and project. Message frequency varies. Msg & data \
         rates may apply. Reply HELP for help, STOP to opt out.\" Privacy policy: \
         {base}/site/privacy"
    );
    let sharing = "No mobile information will be shared with third parties or affiliates for \
                   marketing or promotional purposes.";
    fake.answer_with(ai_answer(
        "Almost there.",
        &[(
            "website",
            json!({ "status": "pass", "evidence": "Family owned since 1962", "note": "", "fixes": [] }),
        )],
    ));
    fake.answer_with(ai_answer(
        "Two changes are needed before the carriers will approve it.",
        &[
            (
                "privacy_no_sharing",
                json!({ "status": "fail", "evidence": "We may share your information with marketing partners.",
                        "note": "", "fixes": [{
                            "location": "page:privacyPolicy",
                            "problem": "Carriers require the policy to say phone numbers are never shared for marketing.",
                            "current": "We may share your information with marketing partners.",
                            "replacement": sharing,
                        }] }),
            ),
            (
                "message_flow",
                json!({ "status": "fail", "evidence": "", "note": "", "fixes": [{
                            "location": "campaign.messageFlow",
                            "problem": "Carriers need the quote form's address and its opt-in words.",
                            "current": "",
                            "replacement": flow,
                        }] }),
            ),
        ],
    ));
    let run = run_checks(&server).await;
    assert_eq!(run["processed"], 1);
    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["model"], api::ai::LARGE_MODEL);
    assert_eq!(requests[0]["response_format"]["type"], "json_schema");
    let read = requests[0]["messages"][1]["content"].as_str().unwrap();
    // The pages as read: the form's fields and words, no scripts; nothing
    // code found on them.
    assert!(
        read.contains("I agree to receive texts from Acme Stone"),
        "{read}"
    );
    assert!(read.contains("type=\\\"checkbox\\\""), "{read}");
    assert!(read.contains("We may share your information"), "{read}");
    assert!(!read.contains("track("), "{read}");
    assert!(read.contains("\"facts\": []"), "{read}");
    let sent_back = requests[1]["messages"].as_array().unwrap();
    assert_eq!(sent_back.len(), 4);
    assert!(
        sent_back[3]["content"]
            .as_str()
            .unwrap()
            .contains("`website`")
    );

    let after: Value = server
        .get("/v1/phone/admin/tendlc")
        .with_session(sessions::MANAGER)
        .await
        .json();
    assert_eq!(after["status"], "draft");
    assert_eq!(after["actions"]["markReady"], true);
    let check = &after["aiCheck"];
    assert_eq!(check["status"], "changes");
    assert_eq!(
        check["summary"],
        "Two changes are needed before the carriers will approve it."
    );
    assert_eq!(
        check["fixes"],
        json!([
            {
                "location": "page:privacyPolicy",
                "label": "Your privacy policy",
                "link": format!("{base}/site/privacy"),
                "problem": "Carriers require the policy to say phone numbers are never shared for marketing.",
                "current": "We may share your information with marketing partners.",
                "replacement": sharing,
            },
            {
                "location": "campaign.messageFlow",
                "label": "How customers opt in",
                "link": null,
                "problem": "Carriers need the quote form's address and its opt-in words.",
                "current": form["campaign"]["messageFlow"],
                "replacement": flow,
            },
        ])
    );
    // Mark gets the changes, his text and the text to use; nobody else.
    let emails = fake.emails();
    assert_eq!(emails.len(), 1);
    let (to, subject, body) = &emails[0];
    assert_eq!(to, &mark);
    assert_eq!(
        subject,
        "[Granite Manager] Your texting registration needs changes"
    );
    assert!(body.contains("We may share your information with marketing partners."));
    assert!(body.contains(sharing));
    assert!(body.contains("/employee/texting-registration/tendlc"));
    assert!(bell(&pool, "tendlc_ready").await.is_empty());

    // Fixed and marked ready again: it passes and goes to the superusers,
    // with what the AI could not verify.
    fake.page("/privacy", FIXED_PRIVACY_PAGE);
    form["campaign"]["messageFlow"] = json!(flow);
    let marked = ready(&server, &form).await;
    assert_eq!(marked["aiCheck"]["status"], "pending");
    fake.answer_with(ai_answer(
        "Everything the carriers check is in place.",
        &[(
            "quote_form_checkbox",
            json!({ "status": "unclear", "evidence": "", "note": "The checkbox is drawn by a script.",
                    "fixes": [] }),
        )],
    ));
    assert_eq!(run_checks(&server).await["processed"], 1);
    let passed: Value = server
        .get("/v1/phone/admin/tendlc")
        .with_session(sessions::MANAGER)
        .await
        .json();
    assert_eq!(passed["status"], "ready");
    assert_eq!(passed["readyByName"], "Mark Manager");
    assert_eq!(passed["aiCheck"]["status"], "passed");
    assert_eq!(passed["aiCheck"]["fixes"], json!([]));
    assert_eq!(
        passed["aiCheck"]["unclear"],
        json!([{ "label": "Opt-in checkbox", "note": "The checkbox is drawn by a script." }])
    );
    let superusers: Vec<i32> = sqlx::query_scalar(
        "SELECT id FROM users WHERE is_superuser = 1 AND is_deleted = 0 ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let told = bell(&pool, "tendlc_ready").await;
    assert_eq!(told.len(), superusers.len());
    assert!(told.iter().all(|(_, message)| message
        == "Acme Stone's texting registration (10DLC) passed the AI check: ready to send to Telnyx."));
    let emails = fake.emails();
    let to_mark: Vec<&(String, String, String)> =
        emails.iter().filter(|(to, _, _)| to == &mark).collect();
    assert_eq!(to_mark.len(), 2);
    assert_eq!(
        to_mark[1].1,
        "[Granite Manager] Your texting registration passed the check"
    );
    let to_sam = emails
        .iter()
        .find(|(to, _, _)| to == &sam)
        .expect("Sam is emailed");
    assert_eq!(
        to_sam.1,
        "[Granite Manager] Acme Stone passed the AI check: ready to send"
    );
    assert!(to_sam.2.contains("The checkbox is drawn by a script."));
    assert!(
        to_sam
            .2
            .contains("/admin/superuser-dashboard/companies/tendlc/100")
    );
    assert_eq!(review(&server).await["actions"]["send"], true);

    // A pass more than a day old is checked again before it is sent: still
    // ready once it passes, and the superusers are told again.
    sql(
        &pool,
        "UPDATE tendlc_registrations SET ai_checked_at = UTC_TIMESTAMP() - INTERVAL 2 DAY
          WHERE company_id = 100",
    )
    .await;
    let stale = superuser_post(&server, "send", &json!({})).await;
    stale.assert_status(StatusCode::CONFLICT);
    assert!(
        stale.json::<Value>()["error"]
            .as_str()
            .unwrap()
            .contains("runs again now")
    );
    let waiting = review(&server).await;
    assert_eq!(waiting["registration"]["aiCheck"]["status"], "pending");
    superuser_post(&server, "send", &json!({}))
        .await
        .assert_status(StatusCode::CONFLICT);
    fake.answer_with(ai_answer("Still in order.", &[]));
    assert_eq!(run_checks(&server).await["processed"], 1);
    let rechecked = review(&server).await;
    assert_eq!(rechecked["registration"]["status"], "ready");
    assert_eq!(rechecked["registration"]["readyByName"], "Mark Manager");
    assert_eq!(rechecked["registration"]["aiCheck"]["status"], "passed");
    assert_eq!(
        bell(&pool, "tendlc_ready").await.len(),
        2 * superusers.len()
    );

    // A Manager's change after it passed takes it back to a draft, to be
    // checked again; nothing waits for the job.
    form["campaign"]["description"] =
        json!("Acme Stone texts customers about their quotes, measuring and installation.");
    let changed: Value = save_form(&server, sessions::MANAGER, &form).await.json();
    assert_eq!(changed["status"], "draft");
    assert_eq!(changed["actions"]["markReady"], true);
    assert_eq!(run_checks(&server).await["processed"], 0);
    assert_eq!(fake.requests().len(), 4);
}

#[test_db::test(seed)]
async fn code_finds_what_the_pages_miss_whatever_the_ai_says(pool: MySqlPool) {
    setup(&pool).await;
    let (server, fake, base) = check_server(&pool).await;
    // The quote form's checkbox is checked in advance, it links to another
    // privacy page and to no terms, and the website shows no phone, email
    // or address.
    fake.page(
        "/quote",
        &QUOTE_PAGE
            .replace(
                "<input type=\"checkbox\" id=\"sms\" name=\"sms\">",
                "<input type=\"checkbox\" id=\"sms\" name=\"sms\" checked>",
            )
            .replace("/site/privacy\"", "/site/privacy-policy\"")
            .replace("<a href=\"/site/terms\">Terms</a>", ""),
    );
    fake.page("/privacy", FIXED_PRIVACY_PAGE);
    fake.page("/privacy-policy", FIXED_PRIVACY_PAGE);
    fake.page("/terms", TERMS_PAGE);
    fake.page(
        "/",
        "<html><body><h1>Acme Stone</h1><p>Granite and quartz countertops.</p></body></html>",
    );
    let mut form = form();
    form["brand"]["website"] = json!(format!("{base}/site/"));
    form["campaign"]["optinFormLink"] = json!(format!("{base}/site/quote"));
    form["campaign"]["privacyPolicyLink"] = json!(format!("{base}/site/privacy"));
    form["campaign"]["termsAndConditionsLink"] = json!(format!("{base}/site/terms"));
    form["campaign"]["messageFlow"] = json!(format!(
        "Customers ask for a quote on our quote form at {base}/site/quote and agree to texts from \
         Acme Stone. Privacy policy: {base}/site/privacy"
    ));
    save_form(&server, sessions::MANAGER, &form)
        .await
        .assert_status_ok();
    ready(&server, &form).await;
    // The AI passes everything.
    fake.answer_with(ai_answer("Everything is in order.", &[]));
    assert_eq!(run_checks(&server).await["processed"], 1);
    let read = fake.requests()[0]["messages"][1]["content"]
        .as_str()
        .unwrap()
        .to_string();
    // The privacy page the quote form links to was read too.
    assert!(read.contains("page:linkedPrivacy"), "{read}");
    assert!(
        read.contains("The texts checkbox is checked in advance."),
        "{read}"
    );

    let after: Value = server
        .get("/v1/phone/admin/tendlc")
        .with_session(sessions::MANAGER)
        .await
        .json();
    assert_eq!(after["aiCheck"]["status"], "changes");
    let fixes: Vec<(String, String)> = after["aiCheck"]["fixes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|fix| {
            (
                fix["location"].as_str().unwrap().to_string(),
                fix["replacement"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let locations: Vec<&str> = fixes
        .iter()
        .map(|(location, _)| location.as_str())
        .collect();
    assert_eq!(
        locations,
        [
            "page:quoteForm",
            "page:quoteForm",
            "campaign.privacyPolicyLink",
            "page:website",
        ]
    );
    assert_eq!(fixes[1].1, format!("Terms: {base}/site/terms"));
    assert_eq!(fixes[2].1, format!("{base}/site/privacy-policy"));
    assert_eq!(
        fixes[3].1,
        "Phone: (317) 555-0100\nEmail: office@acmestone.test\nAddress: 100 Main St, \
         Indianapolis, IN 46204"
    );
}

/// The API with an AI on a stand-in that streams `pieces` as its answer;
/// the AI requests it received.
async fn helper_server(pool: &MySqlPool, pieces: &[&str]) -> (TestServer, Arc<Mutex<Vec<Value>>>) {
    let asked: Arc<Mutex<Vec<Value>>> = Arc::default();
    let seen = asked.clone();
    let body: String = pieces
        .iter()
        .map(|piece| {
            let event = json!({ "choices": [{ "delta": { "content": piece } }] });
            format!("data: {event}\n\n")
        })
        .chain(["data: [DONE]\n\n".to_string()])
        .collect();
    let router = Router::new().fallback(move |request: Request| {
        let seen = seen.clone();
        let body = body.clone();
        async move {
            let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
                .await
                .unwrap();
            seen.lock()
                .unwrap()
                .push(serde_json::from_slice(&bytes).unwrap_or(Value::Null));
            ([("content-type", "text/event-stream")], body).into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN).with_ai(Ai::new(
        Some("ai-key"),
        &format!("http://{address}/openai/chat"),
    ));
    (TestServer::new(app(state)), asked)
}

fn question(text: &str, form: &Value) -> Value {
    json!({
        "messages": [
            { "role": "user", "content": "Hi" },
            { "role": "assistant", "content": "Hello! Ask me about the form." },
            { "role": "user", "content": text }
        ],
        "draft": form,
        "step": "business"
    })
}

#[test_db::test(seed)]
async fn the_helper_answers_from_the_form_on_screen(pool: MySqlPool) {
    setup(&pool).await;
    let (server, asked) = helper_server(&pool, &["Type your ", "EIN\nas 12-3456789."]).await;
    let mut form = form();
    form["brand"]["ein"] = json!("");

    // Mark asks: the answer streams, then `done`.
    let response = server
        .post("/v1/phone/admin/tendlc/chat")
        .with_session(sessions::MANAGER)
        .json(&question("What goes in Tax id?", &form))
        .await;
    response.assert_status_ok();
    let text = response.text();
    assert!(text.contains("data: Type your "), "{text}");
    assert!(text.contains("data: EIN\ndata: as 12-3456789."), "{text}");
    assert!(text.contains("event: done"), "{text}");

    // The AI read the rules, the company, the form on screen with what
    // blocks it, and the conversation.
    let request = asked.lock().unwrap()[0].clone();
    assert_eq!(request["stream"], json!(true));
    let messages = request["messages"].as_array().unwrap();
    let rules = messages[0]["content"].as_str().unwrap();
    assert!(rules.contains("a Manager of the company"));
    assert!(rules.contains("Opt-in checkbox"));
    let context = messages[1]["content"].as_str().unwrap();
    assert!(context.contains("Acme Stone LLC"), "{context}");
    assert!(context.contains("step 1. Business"), "{context}");
    assert!(
        context.contains("brand.ein: Enter the tax id (EIN)."),
        "{context}"
    );
    assert!(context.contains("never saved"), "{context}");
    let roles: Vec<&str> = messages[2..]
        .iter()
        .map(|message| message["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "assistant", "user"]);
    assert_eq!(messages[4]["content"], json!("What goes in Tax id?"));

    // Sam reviews Acme's form: the superusers' helper.
    server
        .post("/v1/superuser/companies/100/tendlc/chat")
        .with_session(sessions::SUPER_ADMIN)
        .json(&question("Is it ready?", &form))
        .await
        .assert_status_ok();
    let request = asked.lock().unwrap()[1].clone();
    assert!(
        request["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("superuser")
    );

    // No question: 400. Not a Manager, not a superuser, no such company.
    let mut answered = question("x", &form);
    answered["messages"] = json!([{ "role": "assistant", "content": "Hello" }]);
    server
        .post("/v1/phone/admin/tendlc/chat")
        .with_session(sessions::MANAGER)
        .json(&answered)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .post("/v1/phone/admin/tendlc/chat")
        .with_session(sessions::ADMIN)
        .json(&question("x", &form))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .post("/v1/superuser/companies/100/tendlc/chat")
        .with_session(sessions::MANAGER)
        .json(&question("x", &form))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .post("/v1/superuser/companies/999/tendlc/chat")
        .with_session(sessions::SUPER_ADMIN)
        .json(&question("x", &form))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    assert_eq!(asked.lock().unwrap().len(), 2);
}
