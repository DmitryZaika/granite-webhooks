//! Porting numbers into Telnyx (`routes/phone/porting.rs`,
//! `routes/telnyx_webhooks/porting.rs`, `phone/porting.rs`, the phone job's
//! porting step): portability, draft orders (and Telnyx's splits), the
//! business details, port date, documents, submit, cancel and delete, the
//! timeline and comments, Telnyx's notifications (signatures, status
//! changes, comments) and the completion that adds the ported numbers.
//!
//! Telnyx is a stateful stand-in below (`Fake`). Numbers: `...0101` FastPort,
//! `...0102` split into its own order, `...0999` not portable, `...0998`
//! refused by Telnyx at creation.

mod common;
mod phone_common;

use api::app;
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
use axum_test::TestServer;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chrono::{Duration, Utc};
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use phone_common::{
    APP_URL, JOB_KEY, PUBLIC_API, Signer, TELNYX_KEY, add_number, enable_calling, hook_path, sql,
};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// One request the stand-in received: method, path and query, JSON body.
type Seen = (String, String, Value);

/// Telnyx's porting API, kept in memory.
#[derive(Clone, Default)]
struct Fake {
    seen: Arc<Mutex<Vec<Seen>>>,
    orders: Arc<Mutex<HashMap<String, Value>>>,
    comments: Arc<Mutex<Vec<Value>>>,
    /// Numbers on our Telnyx account: id, E.164, connection.
    numbers: Arc<Mutex<Vec<(String, String, Option<String>)>>>,
    emails: Arc<Mutex<Vec<Value>>>,
    next: Arc<AtomicUsize>,
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

fn error(status: StatusCode, detail: &str, pointer: Option<&str>) -> Response {
    let mut error = json!({ "code": "10015", "title": "Invalid request", "detail": detail });
    if let Some(pointer) = pointer {
        error["source"] = json!({ "pointer": pointer });
    }
    (status, Json(json!({ "errors": [error] }))).into_response()
}

impl Fake {
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    fn paths(&self) -> Vec<String> {
        self.seen()
            .into_iter()
            .map(|(method, path, _)| format!("{method} {path}"))
            .collect()
    }

    fn bodies(&self, method: &str, prefix: &str) -> Vec<Value> {
        self.seen()
            .into_iter()
            .filter(|(m, path, _)| m == method && path.starts_with(prefix))
            .map(|(_, _, body)| body)
            .collect()
    }

    fn clear(&self) {
        self.seen.lock().unwrap().clear();
    }

    fn order(&self, id: &str) -> Value {
        self.orders.lock().unwrap()[id].clone()
    }

    /// Changes an order the way Telnyx would on its side.
    fn set(&self, id: &str, patch: &Value) {
        merge(self.orders.lock().unwrap().get_mut(id).unwrap(), patch);
    }

    fn add_account_number(&self, id: &str, e164: &str, connection: Option<&str>) {
        self.numbers.lock().unwrap().push((
            id.to_string(),
            e164.to_string(),
            connection.map(str::to_string),
        ));
    }

    fn emails(&self) -> Vec<Value> {
        self.emails.lock().unwrap().clone()
    }

    fn new_order(&self, numbers: &[&Value], reference: &Value) -> Value {
        let n = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        let id = format!("ord-{n}");
        let fast = numbers
            .iter()
            .all(|number| number.as_str().is_some_and(|n| n.ends_with("0101")));
        let order = json!({
            "id": id, "record_type": "porting_order",
            "status": { "value": "draft", "details": [] },
            "customer_reference": reference, "support_key": null,
            "old_service_provider_ocn": "Unreal Communications", "phone_number_type": "local",
            "phone_numbers": numbers.iter().map(|number| json!({
                "phone_number": number.as_str().unwrap().trim_start_matches('+'),
                "activation_status": "New", "portability_status": "confirmed",
                "porting_order_status": "draft", "record_type": "porting_phone_number" }))
                .collect::<Vec<_>>(),
            "activation_settings": { "fast_port_eligible": fast, "foc_datetime_requested": null,
                                     "foc_datetime_actual": null, "activation_status": null },
            "documents": { "loa": null, "invoice": null },
            "end_user": { "admin": { "entity_name": null, "auth_person_name": null,
                                     "billing_phone_number": null, "account_number": null,
                                     "pin_passcode": null },
                          "location": { "street_address": null, "locality": null } },
            "misc": null, "requirements": [], "requirements_met": false,
            "messaging": { "messaging_capable": true, "enable_messaging": false,
                           "messaging_port_status": "not_applicable" },
            "phone_number_configuration": { "connection_id": null, "messaging_profile_id": null },
            "webhook_url": null, "additional_steps": [],
            "updated_at": "2026-10-23T10:00:00Z"
        });
        self.orders.lock().unwrap().insert(id, order.clone());
        order
    }

    fn telnyx(&self, method: &str, uri: &str, body: &Value) -> Response {
        let ok = |data: Value| Json(json!({ "data": data })).into_response();
        let (path, query) = uri.split_once('?').unwrap_or((uri, ""));
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        match (method, parts.as_slice()) {
            ("POST", ["portability_checks"]) => ok(Value::Array(
                body["phone_numbers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|number| {
                        let number = number.as_str().unwrap();
                        let portable = !number.ends_with("0999");
                        json!({ "record_type": "portability_check_result", "phone_number": number,
                                "portable": portable, "fast_portable": number.ends_with("0101"),
                                "not_portable_reason": if portable { "" } else { "No coverage" } })
                    })
                    .collect(),
            )),
            ("POST", ["porting_orders"]) => {
                let numbers: Vec<&Value> =
                    body["phone_numbers"].as_array().unwrap().iter().collect();
                if numbers
                    .iter()
                    .any(|number| number.as_str().unwrap().ends_with("0998"))
                {
                    return error(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "+13175550998 is not portable",
                        Some("/phone_numbers"),
                    );
                }
                let (split, kept): (Vec<&Value>, Vec<&Value>) = numbers
                    .into_iter()
                    .partition(|number| number.as_str().unwrap().ends_with("0102"));
                let mut created = Vec::new();
                for group in [kept, split] {
                    if !group.is_empty() {
                        created.push(self.new_order(&group, &body["customer_reference"]));
                    }
                }
                (StatusCode::CREATED, Json(json!({ "data": created }))).into_response()
            }
            (_, ["porting_orders", id, rest @ ..]) => {
                let id = id.to_string();
                if !self.orders.lock().unwrap().contains_key(&id) {
                    return error(StatusCode::NOT_FOUND, "Resource not found", None);
                }
                self.porting_order(method, &id, rest, query, body)
            }
            ("POST", ["documents"]) => {
                let n = self.next.fetch_add(1, Ordering::SeqCst) + 1;
                ok(json!({ "id": format!("doc-{n}"), "record_type": "document",
                           "filename": body["filename"], "status": "pending" }))
            }
            ("GET", ["phone_numbers"]) => {
                let query =
                    url::form_urlencoded::parse(query.as_bytes()).collect::<HashMap<_, _>>();
                let listed: Vec<Value> = self
                    .numbers
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(_, e164, connection)| {
                        query
                            .get("filter[connection_id]")
                            .is_none_or(|wanted| connection.as_deref() == Some(wanted.as_ref()))
                            && query
                                .get("filter[phone_number]")
                                .is_none_or(|digits| e164.contains(digits.as_ref()))
                    })
                    .map(|(id, e164, connection)| {
                        json!({ "id": id, "phone_number": e164, "connection_id": connection,
                                "messaging_profile_id": null })
                    })
                    .collect();
                Json(json!({ "data": listed, "meta": { "total_pages": 1 } })).into_response()
            }
            ("PATCH", ["phone_numbers", id]) => {
                for number in self.numbers.lock().unwrap().iter_mut() {
                    if number.0 == *id {
                        number.2 = body["connection_id"].as_str().map(str::to_string);
                    }
                }
                ok(json!({ "id": id }))
            }
            _ => error(StatusCode::NOT_FOUND, "Unknown route", None),
        }
    }

    fn porting_order(
        &self,
        method: &str,
        id: &str,
        rest: &[&str],
        _query: &str,
        body: &Value,
    ) -> Response {
        let ok = |data: Value| Json(json!({ "data": data })).into_response();
        let status = self.order(id)["status"]["value"]
            .as_str()
            .unwrap()
            .to_string();
        match (method, rest) {
            ("GET", []) => ok(self.order(id)),
            ("PATCH", []) => {
                if !["draft", "in-process", "exception"].contains(&status.as_str()) {
                    return error(StatusCode::UNPROCESSABLE_ENTITY, "Order cannot be edited", None);
                }
                self.set(id, body);
                let mut answer = self.order(id);
                // Telnyx's PATCH answer carries no numbers.
                answer.as_object_mut().unwrap().remove("phone_numbers");
                ok(answer)
            }
            ("DELETE", []) => {
                self.orders.lock().unwrap().remove(id);
                StatusCode::NO_CONTENT.into_response()
            }
            ("POST", ["actions", "confirm"]) => {
                let order = self.order(id);
                if order["documents"]["loa"].is_null() {
                    return error(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "The LOA is missing",
                        Some("/documents/loa"),
                    );
                }
                self.set(
                    id,
                    &json!({ "status": { "value": "in-process", "details": [] },
                             "support_key": "sr_ab12cd" }),
                );
                ok(self.order(id))
            }
            ("POST", ["actions", "cancel"]) => {
                self.set(id, &json!({ "status": { "value": "cancel-pending", "details": [] } }));
                ok(self.order(id))
            }
            ("GET", ["requirements"]) => Json(json!({ "data": [
                { "field_type": "document", "field_value": null, "requirement_status": "requirement-info-pending",
                  "record_type": "porting_requirement",
                  "requirement_type": { "id": "req-invoice", "name": "Latest Invoice", "type": "document",
                      "description": "A copy of the latest phone bill", "example": "Most recent phone bill",
                      "acceptance_criteria": { "acceptable_values": [] } } },
                { "field_type": "textual", "field_value": "ACME-1", "requirement_status": "approved",
                  "record_type": "porting_requirement",
                  "requirement_type": { "id": "req-code", "name": "Customer code", "type": "textual" } }
            ], "meta": { "total_pages": 1 } }))
            .into_response(),
            ("GET", ["allowed_foc_windows"]) => {
                let day = (Utc::now() + Duration::days(3)).date_naive();
                Json(json!({ "data": [
                    { "started_at": format!("{day}T15:00:00Z"), "ended_at": format!("{day}T23:00:00Z"),
                      "record_type": "porting_order" }] }))
                .into_response()
            }
            ("GET", ["loa_template"]) => (
                [(axum::http::header::CONTENT_TYPE, "application/pdf")],
                b"%PDF-1.4 fake LOA".to_vec(),
            )
                .into_response(),
            ("GET", ["comments"]) => {
                let comments: Vec<Value> = self
                    .comments
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|comment| comment["porting_order_id"] == id)
                    .cloned()
                    .collect();
                Json(json!({ "data": comments, "meta": { "total_pages": 1 } })).into_response()
            }
            ("POST", ["comments"]) => {
                let n = self.next.fetch_add(1, Ordering::SeqCst) + 1;
                let comment = json!({ "id": format!("c-{n}"), "body": body["body"], "porting_order_id": id,
                    "user_type": "user", "created_at": "2026-10-23T11:00:00Z", "record_type": "porting_comment" });
                self.comments.lock().unwrap().push(comment.clone());
                (StatusCode::CREATED, Json(json!({ "data": comment }))).into_response()
            }
            _ => error(StatusCode::NOT_FOUND, "Unknown route", None),
        }
    }

    async fn answer(&self, request: Request) -> Response {
        let (parts, body) = request.into_parts();
        let bytes = axum::body::to_bytes(body, 64 << 20).await.unwrap();
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
        if uri == "/hooks/email/send" {
            self.emails.lock().unwrap().push(json);
            return Json(json!({ "message_id": "<port@ses.test>" })).into_response();
        }
        StatusCode::NOT_FOUND.into_response()
    }
}

/// The API with Telnyx and the webhooks Lambda pointed at the stand-in;
/// Telnyx's notifications signed by `signer` (else unsigned ones pass).
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

/// Starts a port of `numbers` as Ada (admin of company 100): the orders.
async fn start(server: &TestServer, numbers: &[&str], label: &str) -> Value {
    let response = server
        .post("/v1/phone/admin/ports")
        .with_session(sessions::ADMIN)
        .json(&json!({ "phoneNumbers": numbers, "label": label }))
        .await;
    response.assert_status(StatusCode::CREATED);
    response.json::<Value>()["orders"].clone()
}

/// The business details, address and a full port, as the form sends them.
fn details() -> Value {
    json!({
        "endUser": { "entityName": "Acme Granite LLC", "authPersonName": "Ada Admin",
                     "billingPhoneNumber": "317-555-0101", "accountNumber": "8855-1",
                     "pinPasscode": "4321" },
        "location": { "streetAddress": "100 Main St", "extendedAddress": "Suite 5",
                      "locality": "Indianapolis", "administrativeArea": "in",
                      "postalCode": "46204", "countryCode": "US" },
        "portType": { "kind": "full" }
    })
}

fn pdf(text: &str) -> String {
    STANDARD.encode(format!("%PDF-1.4\n{text}"))
}

async fn upload(
    server: &TestServer,
    id: i64,
    kind: &str,
    content: &str,
) -> axum_test::TestResponse {
    server
        .post(&format!("/v1/phone/admin/ports/{id}/documents"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "kind": kind, "filename": format!("{kind}.pdf"), "contentBase64": content }))
        .await
}

/// The company's audit log of ports: (actor, action).
async fn audit_actions(pool: &MySqlPool) -> Vec<(Option<i32>, String)> {
    sqlx::query_as(
        "SELECT actor_user_id, action FROM phone_audit_log
          WHERE company_id = 100 AND target LIKE 'port_order:%' ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// The order's timeline as `kind[:status][:detail]`.
async fn timeline(server: &TestServer, id: i64) -> Vec<String> {
    let body: Value = server
        .get(&format!("/v1/phone/admin/ports/{id}/timeline"))
        .with_session(sessions::ADMIN)
        .await
        .json();
    body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            let mut text = entry["kind"].as_str().unwrap().to_string();
            for field in ["status", "detail"] {
                if let Some(value) = entry[field].as_str() {
                    text.push(':');
                    text.push_str(value);
                }
            }
            text
        })
        .collect()
}

/// Bell notifications about ports: (user, message).
async fn port_notifications(pool: &MySqlPool) -> Vec<(i32, String)> {
    sqlx::query_as(
        "SELECT user_id, message FROM notifications WHERE notification_type = 'phone_port' ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// Posts a Telnyx porting notification for company 100.
async fn notify(
    server: &TestServer,
    signer: Option<&Signer>,
    event: &Value,
) -> axum_test::TestResponse {
    let bytes = serde_json::to_vec(event).unwrap();
    let mut request = server
        .post(&hook_path(100, "porting"))
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

#[test_db::test(seed)]
async fn admins_check_numbers_and_start_ports(pool: MySqlPool) {
    enable_calling(&pool, 100, "billing_group_id = 'bg-100'").await;
    add_number(&pool, 9701, 100, "+13175550150", "direct", "Old line").await;
    let (server, fake) = server(&pool, None).await;
    let checked = server
        .post("/v1/phone/admin/ports/portability")
        .with_session(sessions::ADMIN)
        .json(
            &json!({ "phoneNumbers": ["317-555-0101", "3175550102", "+13175550999",
                                         "(317) 555-0150", "317.555.0101"] }),
        )
        .await;
    checked.assert_status_ok();
    let result = |e164: &str, portable: bool, fast: bool, reason: Value, on_telnyx: bool| {
        json!({ "e164": e164, "portable": portable, "fastPortable": fast, "messagingCapable": null,
                "reason": reason, "onTelnyx": on_telnyx, "inOpenPort": false })
    };
    assert_eq!(
        checked.json::<Value>(),
        json!({ "results": [
            result("+13175550101", true, true, Value::Null, false),
            result("+13175550102", true, false, Value::Null, false),
            result("+13175550999", false, false, json!("No coverage"), false),
            result("+13175550150", true, false, Value::Null, true),
        ] })
    );
    // Numbers go to Telnyx in the body, never in a URL.
    assert_eq!(
        fake.bodies("POST", "/portability_checks"),
        [
            json!({ "phone_numbers": ["+13175550101", "+13175550102", "+13175550999", "+13175550150"] })
        ]
    );
    // A number outside the US and Canada is refused without naming it.
    let refused = server
        .post("/v1/phone/admin/ports/portability")
        .with_session(sessions::ADMIN)
        .json(&json!({ "phoneNumbers": ["+442071838750"] }))
        .await;
    refused.assert_status(StatusCode::BAD_REQUEST);
    assert!(!refused.text().contains("2071838750"));
    // Admins only.
    for session in [sessions::REP, sessions::VIEWER] {
        server
            .get("/v1/phone/admin/ports")
            .with_session(session)
            .await
            .assert_status(StatusCode::FORBIDDEN);
    }
    server
        .get("/v1/phone/admin/ports")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);

    // Telnyx splits the request: 0102 goes to an order of its own.
    fake.clear();
    let orders = start(&server, &["3175550101", "317-555-0102"], "Main line").await;
    assert_eq!(orders.as_array().unwrap().len(), 2);
    let (first, second) = (&orders[0], &orders[1]);
    assert_eq!(first["phoneNumbers"], json!(["+13175550101"]));
    assert_eq!(second["phoneNumbers"], json!(["+13175550102"]));
    assert_eq!(first["requestId"], second["requestId"]);
    assert_eq!(first["status"], "draft");
    assert_eq!(first["label"], "Main line");
    assert_eq!(first["fastPort"], true);
    assert_eq!(first["createdByName"], "Ada Admin");
    assert_eq!(
        first["actions"],
        json!({ "edit": true, "submit": true, "cancel": false, "delete": true, "comment": true })
    );
    let created = fake.bodies("POST", "/porting_orders");
    assert_eq!(
        created[0]["phone_numbers"],
        json!(["+13175550101", "+13175550102"])
    );
    assert!(
        created[0]["customer_reference"]
            .as_str()
            .unwrap()
            .starts_with("granite-c100-")
    );
    // Each order is set up: our webhook, and the TeXML app (and billing
    // group) the numbers land on.
    let setup = fake.bodies("PATCH", "/porting_orders/");
    assert_eq!(setup.len(), 2);
    for body in &setup {
        assert_eq!(
            body,
            &json!({ "webhook_url": format!("{PUBLIC_API}/v1/webhooks/telnyx/{}/porting",
                                            phone_common::webhook_token(100)),
                     "phone_number_configuration": { "connection_id": "app-100",
                                                     "billing_group_id": "bg-100" } })
        );
    }
    // The same numbers cannot start a second port, nor company numbers.
    for (numbers, message) in [
        (
            json!(["3175550101"]),
            "1 of these numbers is in a port already.",
        ),
        (
            json!(["3175550150", "3175550160"]),
            "1 of these numbers is on Telnyx already.",
        ),
    ] {
        let response = server
            .post("/v1/phone/admin/ports")
            .with_session(sessions::ADMIN)
            .json(&json!({ "phoneNumbers": numbers }))
            .await;
        response.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(response.json::<Value>()["error"], message);
    }
    // Telnyx's refusal comes back with its reason, the number masked.
    let refused = server
        .post("/v1/phone/admin/ports")
        .with_session(sessions::ADMIN)
        .json(&json!({ "phoneNumbers": ["3175550998"] }))
        .await;
    refused.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        refused.json::<Value>()["error"],
        "Telnyx did not accept this: [number] is not portable (phone_numbers)"
    );
    // The list, newest first; another company sees none of it.
    let listed: Value = server
        .get("/v1/phone/admin/ports")
        .with_session(sessions::ADMIN)
        .await
        .json();
    let ids: Vec<i64> = listed["orders"]
        .as_array()
        .unwrap()
        .iter()
        .map(|order| order["id"].as_i64().unwrap())
        .collect();
    assert_eq!(
        ids,
        [
            second["id"].as_i64().unwrap(),
            first["id"].as_i64().unwrap()
        ]
    );
    let other: Value = server
        .get("/v1/phone/admin/ports")
        .with_session(sessions::SUPER_ADMIN)
        .await
        .json();
    assert_eq!(other, json!({ "orders": [] }));
    server
        .get(&format!("/v1/phone/admin/ports/{}", first["id"]))
        .with_session(sessions::SUPER_ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    assert_eq!(
        audit_actions(&pool).await,
        [
            (Some(102), "port_created".into()),
            (Some(102), "port_created".into())
        ]
    );
    assert_eq!(
        timeline(&server, first["id"].as_i64().unwrap()).await,
        ["created:draft"]
    );
}

#[test_db::test(seed)]
async fn without_a_telnyx_setup_nothing_is_ported(pool: MySqlPool) {
    let (server, fake) = server(&pool, None).await;
    let response = server
        .post("/v1/phone/admin/ports")
        .with_session(sessions::ADMIN)
        .json(&json!({ "phoneNumbers": ["3175550101"] }))
        .await;
    response.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        response.json::<Value>()["error"],
        "Telnyx is not set up for this company yet. Ask a superuser."
    );
    assert!(fake.paths().is_empty());
}

#[test_db::test(seed)]
async fn admins_fill_in_and_submit_a_port(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    let (server, fake) = server(&pool, None).await;
    let order = start(&server, &["3175550101"], "Main line").await[0].clone();
    let id = order["id"].as_i64().unwrap();
    let telnyx_id = "ord-1";
    let path = format!("/v1/phone/admin/ports/{id}");
    // Opening it reads it from Telnyx.
    let opened: Value = server.get(&path).with_session(sessions::ADMIN).await.json();
    assert_eq!(opened["refreshError"], Value::Null);
    assert_eq!(
        opened["details"]["documents"],
        json!({ "loa": false, "invoice": false })
    );
    assert_eq!(
        opened["details"]["numbers"],
        json!([{ "e164": "+13175550101", "activationStatus": "New", "portabilityStatus": "confirmed" }])
    );
    assert_eq!(opened["details"]["endUser"]["pinSet"], false);
    // What is wrong with the details is said without Telnyx.
    fake.clear();
    for (change, message) in [
        (
            json!({ "location": { "streetAddress": "1 A St", "locality": "Indy", "administrativeArea": "IN",
                               "postalCode": "4620", "countryCode": "US" } }),
            "Enter a valid ZIP or postal code.",
        ),
        (
            json!({ "location": { "streetAddress": "1 A St", "locality": "Paris", "administrativeArea": "IF",
                               "postalCode": "75001", "countryCode": "FR" } }),
            "The address must be in the US or Canada.",
        ),
        (
            json!({ "portType": { "kind": "partial" } }),
            "Say what happens to the numbers that stay with the current carrier.",
        ),
        (
            json!({ "endUser": { "entityName": "Acme", "authPersonName": "Ada", "accountNumber": "1",
                              "billingPhoneNumber": "12" } }),
            "The billing number must be a US or Canada number.",
        ),
        (json!({}), "Nothing to change."),
    ] {
        let response = server
            .patch(&path)
            .with_session(sessions::ADMIN)
            .json(&change)
            .await;
        response.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(response.json::<Value>()["error"], message, "{change}");
    }
    assert!(fake.paths().is_empty());
    // The details are saved in Telnyx (with the order's setup).
    let saved = server
        .patch(&path)
        .with_session(sessions::ADMIN)
        .json(&details())
        .await;
    saved.assert_status_ok();
    let saved: Value = saved.json();
    assert_eq!(
        saved["details"]["endUser"],
        json!({ "entityName": "Acme Granite LLC", "authPersonName": "Ada Admin",
                "billingPhoneNumber": "+13175550101", "accountNumber": "8855-1", "pinSet": true })
    );
    assert_eq!(saved["details"]["location"]["administrativeArea"], "IN");
    assert_eq!(saved["details"]["portType"]["kind"], "full");
    let sent = fake.bodies("PATCH", &format!("/porting_orders/{telnyx_id}"));
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0]["end_user"]["admin"],
        json!({ "entity_name": "Acme Granite LLC", "auth_person_name": "Ada Admin",
                "billing_phone_number": "+13175550101", "account_number": "8855-1",
                "pin_passcode": "4321" })
    );
    assert_eq!(sent[0]["misc"], json!({ "type": "full" }));
    assert_eq!(
        sent[0]["phone_number_configuration"]["connection_id"],
        "app-100"
    );
    // A partial port moving the billing number needs the account's new one.
    let partial = server
        .patch(&path)
        .with_session(sessions::ADMIN)
        .json(&json!({ "portType": { "kind": "partial", "remainingNumbersAction": "keep" } }))
        .await;
    partial.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        partial.json::<Value>()["error"],
        "The billing number is moving: enter the account's new billing number."
    );
    // The port date must be one Telnyx offers.
    let windows: Value = server
        .get(&format!("{path}/foc-windows"))
        .with_session(sessions::ADMIN)
        .await
        .json();
    let start_at = windows["windows"][0]["startAt"]
        .as_str()
        .unwrap()
        .to_string();
    let inside = chrono::DateTime::parse_from_rfc3339(&start_at).unwrap() + Duration::hours(1);
    let outside = inside - Duration::days(1);
    let refused = server
        .patch(&path)
        .with_session(sessions::ADMIN)
        .json(&json!({ "focDatetimeRequested": outside.to_rfc3339() }))
        .await;
    refused.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        refused.json::<Value>()["error"],
        "Pick one of the port dates Telnyx offers."
    );
    let dated: Value = server
        .patch(&path)
        .with_session(sessions::ADMIN)
        .json(&json!({ "focDatetimeRequested": inside.to_rfc3339(), "label": "Front desk" }))
        .await
        .json();
    assert_eq!(
        chrono::DateTime::parse_from_rfc3339(dated["order"]["focRequestedAt"].as_str().unwrap())
            .unwrap(),
        inside
    );
    assert_eq!(dated["order"]["label"], "Front desk");
    // The LOA to sign comes from Telnyx.
    let loa = server
        .get(&format!("{path}/loa-template"))
        .with_session(sessions::ADMIN)
        .await;
    loa.assert_status_ok();
    assert_eq!(loa.header("content-type"), "application/pdf");
    assert!(loa.as_bytes().starts_with(b"%PDF"));
    // Documents: PDFs, JPEGs and PNGs of at most 4 MB.
    let gif = upload(&server, id, "invoice", &STANDARD.encode(b"GIF89a....")).await;
    gif.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(gif.json::<Value>()["error"], "Upload a PDF, JPEG or PNG.");
    let big = upload(
        &server,
        id,
        "invoice",
        &pdf(&"x".repeat(4 * 1024 * 1024 + 10)),
    )
    .await;
    big.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        big.json::<Value>()["error"],
        "The file is too big: upload a PDF, JPEG or PNG of at most 4 MB."
    );
    // Submitting without the signed LOA: Telnyx says what is missing.
    let early = server
        .post(&format!("{path}/submit"))
        .with_session(sessions::ADMIN)
        .await;
    early.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        early.json::<Value>()["error"],
        "Telnyx did not accept this: The LOA is missing (documents.loa)"
    );
    fake.clear();
    let bill = upload(&server, id, "invoice", &pdf("October bill")).await;
    bill.assert_status_ok();
    assert_eq!(
        bill.json::<Value>()["details"]["documents"]["invoice"],
        true
    );
    let signed = upload(&server, id, "loa", &pdf("signed")).await;
    assert_eq!(
        signed.json::<Value>()["details"]["documents"],
        json!({ "loa": true, "invoice": true })
    );
    let uploads = fake.bodies("POST", "/documents");
    assert_eq!(uploads.len(), 2);
    assert_eq!(uploads[0]["filename"], "invoice.pdf");
    assert_eq!(
        STANDARD
            .decode(uploads[0]["file"].as_str().unwrap())
            .unwrap(),
        b"%PDF-1.4\nOctober bill"
    );
    assert_eq!(
        fake.order(telnyx_id)["documents"],
        json!({ "loa": "doc-3", "invoice": "doc-2" })
    );
    // Requirements, as Telnyx lists them.
    let requirements: Value = server
        .get(&format!("{path}/requirements"))
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(
        requirements["requirements"][0]["requirementTypeId"],
        "req-invoice"
    );
    assert_eq!(requirements["requirements"][0]["fulfilled"], false);
    assert_eq!(requirements["requirements"][1]["value"], "ACME-1");
    // Submitted.
    let submitted = server
        .post(&format!("{path}/submit"))
        .with_session(sessions::ADMIN)
        .await;
    submitted.assert_status_ok();
    let submitted: Value = submitted.json();
    assert_eq!(submitted["order"]["status"], "in-process");
    assert_eq!(submitted["order"]["supportKey"], "sr_ab12cd");
    assert!(submitted["order"]["submittedAt"].is_string());
    assert_eq!(
        submitted["order"]["actions"],
        json!({ "edit": true, "submit": false, "cancel": true, "delete": false, "comment": true })
    );
    server
        .post(&format!("{path}/submit"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::CONFLICT);
    // Comments with Telnyx's porting team.
    let comment = server
        .post(&format!("{path}/comments"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "body": "The bill is attached." }))
        .await;
    comment.assert_status(StatusCode::CREATED);
    assert_eq!(comment.json::<Value>()["author"], "us");
    let comments: Value = server
        .get(&format!("{path}/comments"))
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(comments["comments"][0]["body"], "The bill is attached.");
    assert_eq!(
        timeline(&server, id).await,
        [
            "created:draft",
            "details:business details, service address, port type",
            "details:port date, label",
            "document:invoice",
            "document:loa",
            "submitted",
            "status:in-process",
            "comment:us",
        ]
    );
    assert_eq!(
        audit_actions(&pool)
            .await
            .into_iter()
            .map(|(_, action)| action)
            .collect::<Vec<_>>(),
        [
            "port_created",
            "port_updated",
            "port_updated",
            "port_document_uploaded",
            "port_document_uploaded",
            "port_submitted",
            "port_commented"
        ]
    );
    // The PIN is never read back, and no bell rang for our own changes.
    assert!(!submitted.to_string().contains("4321"));
    assert!(port_notifications(&pool).await.is_empty());
}

#[test_db::test(seed)]
async fn drafts_are_deleted_and_submitted_ports_cancelled(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    let (server, fake) = server(&pool, None).await;
    let orders = start(&server, &["3175550101", "3175550102"], "").await;
    let (draft, other) = (
        orders[0]["id"].as_i64().unwrap(),
        orders[1]["id"].as_i64().unwrap(),
    );
    let deleted = server
        .delete(&format!("/v1/phone/admin/ports/{draft}"))
        .with_session(sessions::ADMIN)
        .await;
    deleted.assert_status_ok();
    assert_eq!(deleted.json::<Value>()["order"]["status"], "deleted");
    assert!(
        fake.paths()
            .contains(&"DELETE /porting_orders/ord-1".to_string())
    );
    server
        .delete(&format!("/v1/phone/admin/ports/{draft}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::CONFLICT);
    // A deleted draft frees its numbers for a new port.
    start(&server, &["3175550101"], "").await;
    // A submitted port is cancelled, unless its date is under 48 hours away.
    fake.set(
        "ord-2",
        &json!({ "status": { "value": "submitted", "details": [] } }),
    );
    let path = format!("/v1/phone/admin/ports/{other}");
    server
        .post(&format!("{path}/cancel"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::CONFLICT); // still a draft for us
    server
        .get(&path)
        .with_session(sessions::ADMIN)
        .await
        .assert_status_ok();
    sql(&pool, &format!("UPDATE phone_port_orders SET foc_at = UTC_TIMESTAMP() + INTERVAL 1 DAY WHERE id = {other}")).await;
    let close = server
        .post(&format!("{path}/cancel"))
        .with_session(sessions::ADMIN)
        .await;
    close.assert_status(StatusCode::BAD_REQUEST);
    assert!(
        close.json::<Value>()["error"]
            .as_str()
            .unwrap()
            .starts_with("The port date is less than 48 hours away")
    );
    sql(
        &pool,
        &format!("UPDATE phone_port_orders SET foc_at = NULL WHERE id = {other}"),
    )
    .await;
    let cancelled = server
        .post(&format!("{path}/cancel"))
        .with_session(sessions::ADMIN)
        .await;
    cancelled.assert_status_ok();
    assert_eq!(
        cancelled.json::<Value>()["order"]["status"],
        "cancel-pending"
    );
    assert_eq!(
        audit_actions(&pool)
            .await
            .into_iter()
            .map(|(_, action)| action)
            .collect::<Vec<_>>(),
        [
            "port_created",
            "port_created",
            "port_deleted",
            "port_created",
            "port_cancel_requested"
        ]
    );
}

#[test_db::test(seed)]
async fn telnyx_notifications_move_ports_and_ported_numbers_are_added(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    let signer = Signer::new();
    let (server, fake) = server(&pool, Some(&signer)).await;
    let order = start(&server, &["3175550101"], "Main line").await[0].clone();
    let id = order["id"].as_i64().unwrap();
    let changed = |event_id: &str| {
        event(
            event_id,
            "porting_order.status_changed",
            json!({ "id": "ord-1", "status": { "value": "ignored-we-ask-telnyx" } }),
        )
    };
    // Unsigned, badly signed or for an unknown token: refused.
    notify(&server, None, &changed("evt-0"))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    let other = Signer::new();
    notify(&server, Some(&other), &changed("evt-0"))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    server
        .post("/v1/webhooks/telnyx/nobody/porting")
        .json(&changed("evt-0"))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    // The date is confirmed: what Telnyx says when asked is what counts.
    let foc = (Utc::now() + Duration::days(5)).date_naive();
    fake.set(
        "ord-1",
        &json!({ "status": { "value": "foc-date-confirmed", "details": [] }, "support_key": "sr_1",
                 "activation_settings": { "foc_datetime_actual": format!("{foc}T14:00:00Z") } }),
    );
    notify(&server, Some(&signer), &changed("evt-1"))
        .await
        .assert_status_ok();
    // Telnyx sends it again: nothing happens twice.
    notify(&server, Some(&signer), &changed("evt-1"))
        .await
        .assert_status_ok();
    let opened: Value = server
        .get("/v1/phone/admin/ports")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(opened["orders"][0]["status"], "foc-date-confirmed");
    assert_eq!(opened["orders"][0]["focAt"], format!("{foc}T14:00:00.000Z"));
    let bells = port_notifications(&pool).await;
    assert!(!bells.is_empty());
    assert!(bells.iter().any(|(user, _)| *user == 102));
    assert!(
        bells[0]
            .1
            .starts_with("The port of “Main line” (1 number) to Telnyx is confirmed for")
    );
    assert!(bells.iter().all(|(_, message)| !message.contains("0101")));
    // It shows in the admin's bell (it names no deal), opening the Phone page.
    let bell: Value = server
        .get("/v1/notifications")
        .with_session(sessions::ADMIN)
        .await
        .json();
    let ports: Vec<&Value> = bell["notifications"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["notification_type"] == "phone_port")
        .collect();
    assert_eq!(ports.len(), 1);
    assert_eq!(
        (
            &ports[0]["title"],
            &ports[0]["type_title"],
            &ports[0]["href"],
            &ports[0]["customer_name"]
        ),
        (
            &json!("Number port"),
            &json!("Number Port"),
            &json!("/admin/phone"),
            &Value::Null
        )
    );
    assert!(
        ports[0]["message"]
            .as_str()
            .unwrap()
            .starts_with("The port of “Main line” (1 number) to Telnyx is confirmed for")
    );
    let emails = fake.emails();
    assert_eq!(emails.len(), bells.len());
    assert_eq!(emails[0]["subject"], "[Granite Phone] Port date confirmed");
    assert!(
        emails[0]["body"]
            .as_str()
            .unwrap()
            .contains(&format!("https://acme.crm.test/admin/phone/ports/{id}"))
    );
    // A comment from Telnyx's porting team rings the bell; our own does not.
    let comment = |event_id: &str, user_type: &str| {
        event(
            event_id,
            "porting_order.new_comment",
            json!({ "porting_order_id": "ord-1", "support_key": "sr_1",
                      "comment": { "id": "c-9", "body": "Please send a newer bill", "user_type": user_type } }),
        )
    };
    notify(&server, Some(&signer), &comment("evt-2", "admin"))
        .await
        .assert_status_ok();
    notify(&server, Some(&signer), &comment("evt-3", "user"))
        .await
        .assert_status_ok();
    let bells_now = port_notifications(&pool).await;
    assert_eq!(bells_now.len(), bells.len() * 2);
    assert!(
        bells_now
            .last()
            .unwrap()
            .1
            .starts_with("Telnyx's porting team wrote about the port")
    );
    // An order of another company is ignored.
    notify(
        &server,
        Some(&signer),
        &event(
            "evt-4",
            "porting_order.status_changed",
            json!({ "id": "ord-999" }),
        ),
    )
    .await
    .assert_status_ok();
    // Ported: the number becomes a company line on the TeXML app.
    fake.add_account_number("tn-1", "+13175550101", Some("old-connection"));
    fake.set(
        "ord-1",
        &json!({ "status": { "value": "ported", "details": [] },
                               "messaging": { "messaging_port_status": "ported" } }),
    );
    fake.clear();
    notify(&server, Some(&signer), &changed("evt-5"))
        .await
        .assert_status_ok();
    let number: (i32, String, String, bool, Option<String>) = sqlx::query_as(
        "SELECT company_id, label, CAST(kind AS CHAR), active <> 0, telnyx_number_id
           FROM telnyx_numbers WHERE e164 = '+13175550101'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        number,
        (
            100,
            "Main line".into(),
            "direct".into(),
            true,
            Some("tn-1".into())
        )
    );
    let assigned: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM telnyx_number_users nu JOIN telnyx_numbers n ON n.id = nu.number_id WHERE n.e164 = '+13175550101'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(assigned, 0);
    assert_eq!(
        fake.bodies("PATCH", "/phone_numbers/tn-1"),
        [json!({ "connection_id": "app-100" })]
    );
    let done: Value = server
        .get(&format!("/v1/phone/admin/ports/{id}"))
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(done["order"]["status"], "ported");
    assert!(done["order"]["completedAt"].is_string());
    assert_eq!(done["order"]["lastError"], Value::Null);
    assert_eq!(done["order"]["messagingStatus"], "ported");
    assert_eq!(
        done["order"]["actions"],
        json!({ "edit": false, "submit": false, "cancel": false, "delete": false, "comment": true })
    );
    // It shows on the Phone page with the company's numbers.
    let page: Value = server
        .get("/v1/phone/admin/numbers")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert!(
        page["numbers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["e164"] == "+13175550101")
    );
    assert!(
        port_notifications(&pool)
            .await
            .last()
            .unwrap()
            .1
            .starts_with("The port of “Main line” (1 number) is done")
    );
    assert_eq!(
        timeline(&server, id).await,
        [
            "created:draft",
            "status:foc-date-confirmed",
            "comment:telnyx",
            "messaging:ported",
            "status:ported",
            "completed:1"
        ]
    );
    let actions = audit_actions(&pool).await;
    assert_eq!(
        actions.last().unwrap(),
        &(None, "port_completed".to_string())
    );
    // Changes are refused once ported.
    server
        .patch(&format!("/v1/phone/admin/ports/{id}"))
        .with_session(sessions::ADMIN)
        .json(&details())
        .await
        .assert_status(StatusCode::CONFLICT);
}

#[test_db::test(seed)]
async fn the_job_refreshes_ports_and_finishes_ported_numbers(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    let (server, fake) = server(&pool, None).await;
    let order = start(&server, &["3175550101"], "").await[0].clone();
    let id = order["id"].as_i64().unwrap();
    let run = || async {
        let response = server
            .post("/v1/jobs/phone")
            .add_header("authorization", JOB_KEY)
            .await;
        response.assert_status_ok();
        response.json::<Value>()
    };
    // Fresh orders wait; a stale one is read again and its exception told.
    fake.set("ord-1", &json!({ "status": { "value": "exception", "details": [
        { "code": "ACCOUNT_NUMBER_MISMATCH", "description": "Account number does not match that on the CSR" }] } }));
    let first = run().await;
    assert_eq!(first.get("port_orders_refreshed"), None);
    sql(&pool, &format!("UPDATE phone_port_orders SET refreshed_at = UTC_TIMESTAMP() - INTERVAL 7 HOUR WHERE id = {id}")).await;
    let second = run().await;
    assert_eq!(second["port_orders_refreshed"], 1);
    let listed: Value = server
        .get("/v1/phone/admin/ports")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(listed["orders"][0]["status"], "exception");
    assert_eq!(
        listed["orders"][0]["statusDetails"],
        json!([{ "code": "ACCOUNT_NUMBER_MISMATCH", "description": "Account number does not match that on the CSR" }])
    );
    assert!(
        port_notifications(&pool).await[0]
            .1
            .starts_with("Telnyx found a problem with the port of 1 number.")
    );
    // Ported, but Telnyx does not list the number yet: added, not finished.
    fake.set(
        "ord-1",
        &json!({ "status": { "value": "ported", "details": [] } }),
    );
    sql(&pool, &format!("UPDATE phone_port_orders SET refreshed_at = UTC_TIMESTAMP() - INTERVAL 2 HOUR WHERE id = {id}")).await;
    let third = run().await;
    assert_eq!(third["port_orders_refreshed"], 1);
    assert_eq!(third.get("port_orders_completed"), None);
    let (completed, error): (Option<chrono::NaiveDateTime>, Option<String>) =
        sqlx::query_as("SELECT completed_at, last_error FROM phone_port_orders WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(completed, None);
    assert!(
        error
            .unwrap()
            .contains("Telnyx is still finishing the port")
    );
    let added: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM telnyx_numbers WHERE e164 = '+13175550101' AND company_id = 100",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(added, 1);
    // Once Telnyx lists it (on the TeXML app already), the next try finishes.
    fake.add_account_number("tn-7", "+13175550101", Some("app-100"));
    sql(&pool, &format!("UPDATE phone_port_orders SET completion_tried_at = UTC_TIMESTAMP() - INTERVAL 1 DAY WHERE id = {id}")).await;
    let fourth = run().await;
    assert_eq!(fourth["port_orders_completed"], 1);
    // Already on the app: nothing to change on Telnyx.
    assert!(fake.bodies("PATCH", "/phone_numbers/").is_empty());
    let telnyx_id: Option<String> = sqlx::query_scalar(
        "SELECT telnyx_number_id FROM telnyx_numbers WHERE e164 = '+13175550101'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(telnyx_id.as_deref(), Some("tn-7"));
    let fifth = run().await;
    assert_eq!(fifth.get("port_orders_completed"), None);
}
