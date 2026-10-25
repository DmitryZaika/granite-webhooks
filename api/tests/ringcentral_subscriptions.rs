//! `POST /v1/jobs/contact-sync` moves `RingCentral` SMS subscriptions onto
//! the current webhooks token, against a stand-in `RingCentral`.

mod common;

use ::common::access_token::token_hash;
use ::common::telephony::ringcentral::SubscriptionTarget;
use api::app;
use api::state::AppState;
use axum::extract::{Path, State};
use axum::http::Method;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex};

const HOOKS: &str = "https://hooks.test";
const TOKEN: &str = "gmk_abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";

/// What a stand-in was asked: `METHOD path`, and the body of a `PUT`/`POST`.
type Seen = Arc<Mutex<Vec<(String, Value)>>>;

/// A company's `RingCentral`: a JWT token, one enabled extension, and the
/// subscriptions at `addresses` (ids `sub-0`, `sub-1`...).
async fn ringcentral(addresses: &[String]) -> (String, Seen) {
    let seen: Seen = Arc::default();
    let records: Vec<Value> = addresses
        .iter()
        .enumerate()
        .map(|(i, address)| json!({ "id": format!("sub-{i}"), "deliveryMode": { "address": address } }))
        .collect();
    let note = |seen: &Seen, method: Method, path: &str, body: Value| {
        seen.lock()
            .unwrap()
            .push((format!("{method} {path}"), body));
    };
    let router =
        Router::new()
            .route(
                "/restapi/oauth/token",
                post(|| async { Json(json!({ "access_token": "rc-access", "expires_in": 3600 })) }),
            )
            .route(
                "/restapi/v1.0/subscription",
                get(move |State(seen): State<Seen>| {
                    let records = records.clone();
                    async move {
                        note(&seen, Method::GET, "subscription", Value::Null);
                        Json(json!({ "records": records }))
                    }
                })
                .post(
                    move |State(seen): State<Seen>, Json(body): Json<Value>| async move {
                        note(&seen, Method::POST, "subscription", body);
                        Json(json!({ "id": "sub-new" }))
                    },
                ),
            )
            .route(
                "/restapi/v1.0/subscription/{id}",
                put(
                    move |State(seen): State<Seen>,
                          Path(id): Path<String>,
                          Json(body): Json<Value>| async move {
                        note(&seen, Method::PUT, &format!("subscription/{id}"), body);
                        Json(json!({ "id": id }))
                    },
                ),
            )
            .route(
                "/restapi/v1.0/account/~/extension",
                get(move |State(seen): State<Seen>| async move {
                    note(&seen, Method::GET, "extension", Value::Null);
                    Json(json!({ "records": [{ "id": 501 }] }))
                }),
            )
            .with_state(Arc::clone(&seen));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (base, seen)
}

/// A company connected to `RingCentral` through JWT credentials on `server`.
async fn company(pool: &MySqlPool, id: i32, server: &str) {
    sqlx::query(
        "INSERT INTO company (id, name, ringcentral_client_id, ringcentral_client_secret,
                              ringcentral_jwt, ringcentral_server_url)
         VALUES (?, 'RC Co', 'client', 'secret', 'jwt', ?)",
    )
    .bind(id)
    .bind(server)
    .execute(pool)
    .await
    .unwrap();
}

async fn stored_hash(pool: &MySqlPool, id: i32) -> Option<String> {
    sqlx::query_scalar("SELECT ringcentral_subscription_token_hash FROM company WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn server(pool: MySqlPool, target: Option<SubscriptionTarget>) -> TestServer {
    let state = AppState::new(pool, SECRET, ALLOWED_ORIGIN)
        .with_job_key("job-secret")
        .with_ringcentral_subscriptions(target);
    TestServer::new(app(state))
}

async fn run(server: &TestServer) -> Value {
    server
        .post("/v1/jobs/contact-sync")
        .add_header("Authorization", "job-secret")
        .await
        .json()
}

fn requests(seen: &Seen) -> Vec<String> {
    seen.lock()
        .unwrap()
        .iter()
        .map(|(request, _)| request.clone())
        .collect()
}

#[test_db::test]
async fn subscriptions_move_to_the_current_token(pool: MySqlPool) {
    let target = SubscriptionTarget::new(Some(HOOKS), Some(TOKEN), None).unwrap();
    let ours = target.address(7101);
    // Subscribed here (and from elsewhere): renewed with the new token.
    let (subscribed, subscribed_seen) = ringcentral(&[
        "https://elsewhere.test/ringcentral/notify/7101".into(),
        ours.clone(),
    ])
    .await;
    company(&pool, 7101, &subscribed).await;
    // Subscribed only from another copy of the database: left alone.
    let (other, other_seen) =
        ringcentral(&["https://elsewhere.test/ringcentral/notify/7102".into()]).await;
    company(&pool, 7102, &other).await;
    let server = server(pool.clone(), Some(target));

    let body = run(&server).await;
    assert_eq!(body["ringcentral_subscriptions"], 1, "{body}");

    assert_eq!(
        requests(&subscribed_seen),
        [
            "GET subscription",
            "GET extension",
            "PUT subscription/sub-1"
        ]
    );
    let renewal = subscribed_seen.lock().unwrap()[2].1.clone();
    assert_eq!(renewal["deliveryMode"]["address"], ours.as_str());
    assert_eq!(renewal["deliveryMode"]["verificationToken"], TOKEN);
    assert_eq!(
        renewal["eventFilters"][0],
        "/restapi/v1.0/account/~/extension/501/message-store/instant?type=SMS"
    );
    // Never created here.
    assert_eq!(requests(&other_seen), ["GET subscription"]);

    let current = Some(token_hash(TOKEN));
    assert_eq!(stored_hash(&pool, 7101).await, current);
    assert_eq!(stored_hash(&pool, 7102).await, current);

    // Both are on the current token now: nothing more to do.
    let body = run(&server).await;
    assert_eq!(body["ringcentral_subscriptions"], 0, "{body}");
    assert_eq!(requests(&subscribed_seen).len(), 3);
    assert_eq!(requests(&other_seen).len(), 1);
}

#[test_db::test]
async fn a_new_token_reaches_subscriptions_on_the_old_one(pool: MySqlPool) {
    let target = SubscriptionTarget::new(Some(HOOKS), Some(TOKEN), None).unwrap();
    let (server_url, seen) = ringcentral(&[target.address(7103)]).await;
    company(&pool, 7103, &server_url).await;
    sqlx::query("UPDATE company SET ringcentral_subscription_token_hash = ? WHERE id = 7103")
        .bind(token_hash("9ca4dfa80eec46cc967f3385624be883"))
        .execute(&pool)
        .await
        .unwrap();

    let body = run(&server(pool.clone(), Some(target))).await;
    assert_eq!(body["ringcentral_subscriptions"], 1, "{body}");
    let renewal = seen.lock().unwrap().last().unwrap().1.clone();
    assert_eq!(renewal["deliveryMode"]["verificationToken"], TOKEN);
    assert_eq!(stored_hash(&pool, 7103).await, Some(token_hash(TOKEN)));
}

#[test_db::test]
async fn nothing_is_renewed_while_subscriptions_are_off(pool: MySqlPool) {
    let (server_url, seen) = ringcentral(&[format!("{HOOKS}/ringcentral/notify/7104")]).await;
    company(&pool, 7104, &server_url).await;

    let body = run(&server(pool.clone(), None)).await;
    assert_eq!(body["ringcentral_subscriptions"], 0, "{body}");
    assert!(requests(&seen).is_empty());
    assert_eq!(stored_hash(&pool, 7104).await, None);
}
