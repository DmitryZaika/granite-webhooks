//! "Connect Telnyx" and buying numbers (`routes/phone/connect.rs`): a
//! company sets itself up in Telnyx from its own Phone page, and buys
//! numbers there. Telnyx is a stand-in (`phone_common`).
//!
//! Fixture numbers 9951+.

mod common;
mod phone_common;

use axum::http::StatusCode;
use common::{WithSession, sessions};
use phone_common::{PUBLIC_API, REFUSED_NUMBER, Stubs, add_number, phone_server, sql};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::atomic::Ordering;

const CONNECT: &str = "/v1/phone/admin/connect";

/// The company's Telnyx ids: billing group, voice profile, TeXML app,
/// credential connection, messaging profile.
async fn ids(pool: &MySqlPool, company_id: i32) -> [Option<String>; 5] {
    let row: (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT billing_group_id, outbound_voice_profile_id, texml_app_id,
                credential_connection_id, messaging_profile_id
           FROM telnyx_company_config WHERE company_id = ?",
    )
    .bind(company_id)
    .fetch_one(pool)
    .await
    .unwrap();
    [row.0, row.1, row.2, row.3, row.4]
}

/// The root of the company's Telnyx callbacks (its token is random).
async fn webhook_base(pool: &MySqlPool, company_id: i32) -> String {
    let token: String =
        sqlx::query_scalar("SELECT webhook_token FROM telnyx_company_config WHERE company_id = ?")
            .bind(company_id)
            .fetch_one(pool)
            .await
            .unwrap();
    format!("{PUBLIC_API}/v1/webhooks/telnyx/{token}")
}

/// The body Telnyx got for the first `METHOD path` request.
fn sent(stubs: &Stubs, method: &str, path: &str) -> Value {
    stubs
        .telnyx()
        .into_iter()
        .find(|(m, p, _)| m == method && p == path)
        .map(|(_, _, body)| body)
        .unwrap_or_else(|| panic!("no {method} {path} in {:?}", stubs.telnyx_paths()))
}

async fn audit_actions(pool: &MySqlPool, company_id: i32) -> Vec<String> {
    sqlx::query_scalar("SELECT action FROM phone_audit_log WHERE company_id = ? ORDER BY id")
        .bind(company_id)
        .fetch_all(pool)
        .await
        .unwrap()
}

#[test_db::test(seed)]
async fn admins_connect_telnyx_in_one_click(pool: MySqlPool) {
    let (server, stubs) = phone_server(pool.clone()).await;
    // A number Telnyx lists on the company's TeXML app once it exists.
    stubs.add_telnyx_number("num-1", "+13175550151");
    // Not an admin, or not signed in.
    server
        .post(CONNECT)
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .post(CONNECT)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    assert!(stubs.telnyx().is_empty());

    let first = server.post(CONNECT).with_session(sessions::ADMIN).await;
    first.assert_status_ok();
    assert_eq!(
        first.json::<Value>(),
        json!({
            "configured": true,
            "created": ["billingGroup", "voiceProfile", "texmlApp", "credentialConnection",
                        "messagingProfile"],
            "imported": 1,
        })
    );
    let [group, profile, app, connection, messaging] = ids(&pool, 100).await;
    let (group, profile, app, connection, messaging) = (
        group.unwrap(),
        profile.unwrap(),
        app.unwrap(),
        connection.unwrap(),
        messaging.unwrap(),
    );
    // Telnyx's numeric voice profile id is kept as text.
    assert!(profile.chars().all(|c| c.is_ascii_digit()), "{profile}");
    let enabled: bool =
        sqlx::query_scalar("SELECT enabled <> 0 FROM telnyx_company_config WHERE company_id = 100")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(enabled, "a first connect switches calling on");

    let base = webhook_base(&pool, 100).await;
    let group_body = sent(&stubs, "POST", "/billing_groups");
    assert!(group_body["name"].as_str().unwrap().contains("(#100)"));
    let profile_body = sent(&stubs, "POST", "/outbound_voice_profiles");
    assert_eq!(profile_body["billing_group_id"], json!(group));
    assert_eq!(
        profile_body["whitelisted_destinations"],
        json!(["US", "CA"])
    );
    assert_eq!(profile_body["daily_spend_limit_enabled"], true);
    let app_body = sent(&stubs, "POST", "/texml_applications");
    assert!(
        app_body["friendly_name"]
            .as_str()
            .unwrap()
            .starts_with("gm-100-")
    );
    assert_eq!(app_body["voice_url"], format!("{base}/inbound"));
    assert_eq!(app_body["status_callback"], format!("{base}/call-status"));
    assert_eq!(
        app_body["outbound"]["outbound_voice_profile_id"],
        json!(profile)
    );
    let connection_body = sent(&stubs, "POST", "/credential_connections");
    assert_eq!(
        connection_body["webhook_event_url"],
        format!("{base}/outbound")
    );
    assert_eq!(connection_body["webhook_api_version"], "2");
    assert_eq!(connection_body["sip_uri_calling_preference"], "internal");
    assert_eq!(connection_body["outbound"]["call_parking_enabled"], true);
    // The app's own push credential, by alias, not another app's.
    assert_eq!(
        connection_body["android_push_credential_id"],
        "push-android"
    );
    assert!(connection_body.get("ios_push_credential_id").is_none());
    assert!(
        connection_body["user_name"]
            .as_str()
            .unwrap()
            .chars()
            .all(|c| c.is_ascii_alphanumeric())
    );
    let messaging_body = sent(&stubs, "POST", "/messaging_profiles");
    assert_eq!(messaging_body["webhook_url"], format!("{base}/messaging"));
    assert_eq!(messaging_body["mms_transcoding"], true);
    let opt_out = sent(
        &stubs,
        "POST",
        &format!("/messaging_profiles/{messaging}/autoresp_configs"),
    );
    assert_eq!(opt_out["op"], "stop");
    assert!(
        opt_out["keywords"]
            .as_array()
            .unwrap()
            .contains(&json!("REVOKE"))
    );
    // The TeXML app's number came in.
    let imported: Option<String> = sqlx::query_scalar(
        "SELECT telnyx_number_id FROM telnyx_numbers WHERE company_id = 100 AND e164 = '+13175550151'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(imported.as_deref(), Some("num-1"));

    // Again: nothing new is made; everything is pointed at us again and
    // the company's numbers put on the app and the profile.
    stubs.clear_telnyx();
    let again = server.post(CONNECT).with_session(sessions::ADMIN).await;
    again.assert_status_ok();
    assert_eq!(
        again.json::<Value>(),
        json!({ "configured": true, "created": [], "imported": 0 })
    );
    let paths = stubs.telnyx_paths();
    for path in [
        format!("PATCH /texml_applications/{app}"),
        format!("PATCH /credential_connections/{connection}"),
        format!("PATCH /messaging_profiles/{messaging}"),
        "PATCH /phone_numbers/num-1".to_string(),
        "PATCH /phone_numbers/num-1/messaging".to_string(),
    ] {
        assert!(paths.contains(&path), "{path} in {paths:?}");
    }
    assert!(
        !paths.iter().any(|path| path.starts_with("POST")),
        "{paths:?}"
    );
    let app_patch = sent(&stubs, "PATCH", &format!("/texml_applications/{app}"));
    assert!(app_patch.get("friendly_name").is_none(), "the name stays");
    assert_eq!(app_patch["voice_url"], format!("{base}/inbound"));
    assert_eq!(
        sent(&stubs, "PATCH", "/phone_numbers/num-1"),
        json!({ "connection_id": app, "billing_group_id": group })
    );
    assert_eq!(
        sent(&stubs, "PATCH", "/phone_numbers/num-1/messaging"),
        json!({ "messaging_profile_id": messaging })
    );

    // A superuser's switch-off stays.
    sql(
        &pool,
        "UPDATE telnyx_company_config SET enabled = 0 WHERE company_id = 100",
    )
    .await;
    server
        .post(CONNECT)
        .with_session(sessions::ADMIN)
        .await
        .assert_status_ok();
    let enabled: bool =
        sqlx::query_scalar("SELECT enabled <> 0 FROM telnyx_company_config WHERE company_id = 100")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!enabled);
    assert_eq!(
        audit_actions(&pool, 100).await,
        [
            "numbers_imported",
            "telnyx_connected",
            "telnyx_connected",
            "telnyx_connected"
        ]
    );
    // The admin page sees it set up.
    let status = server
        .get("/v1/phone/admin/numbers")
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>()["config"]
        .clone();
    assert_eq!(status["configured"], true);
}

#[test_db::test(seed)]
async fn connecting_repairs_what_was_deleted_and_picks_free_names(pool: MySqlPool) {
    let (server, stubs) = phone_server(pool.clone()).await;
    // Set up by hand once; its TeXML app was deleted in Telnyx since.
    sql(
        &pool,
        "INSERT INTO telnyx_company_config
           (company_id, enabled, webhook_token, billing_group_id, outbound_voice_profile_id,
            texml_app_id, credential_connection_id, messaging_profile_id)
         VALUES (100, 1, REPEAT('c', 32), 'bg-1', 'ovp-1', 'gone', 'conn-1', 'mp-1')",
    )
    .await;
    let name: String = sqlx::query_scalar("SELECT name FROM company WHERE id = 100")
        .fetch_one(&pool)
        .await
        .unwrap();
    // The app's name is in use (an old app of another environment).
    let lowered = name.to_lowercase();
    let words: Vec<&str> = lowered
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    let slug: String = words.join("-").chars().take(40).collect();
    stubs.take_name(&format!("gm-100-{}", slug.trim_end_matches('-')));

    let response = server.post(CONNECT).with_session(sessions::ADMIN).await;
    response.assert_status_ok();
    assert_eq!(response.json::<Value>()["created"], json!(["texmlApp"]));
    let posts: Vec<Value> = stubs
        .telnyx()
        .into_iter()
        .filter(|(method, path, _)| method == "POST" && path == "/texml_applications")
        .map(|(_, _, body)| body)
        .collect();
    assert_eq!(posts.len(), 2, "taken, then with a random ending");
    let first = posts[0]["friendly_name"].as_str().unwrap();
    let retried = posts[1]["friendly_name"].as_str().unwrap();
    assert!(
        retried.starts_with(&format!("{first}-")) && retried.len() == first.len() + 5,
        "{retried}"
    );
    let [group, profile, app, connection, messaging] = ids(&pool, 100).await;
    assert_eq!(
        (
            group.as_deref(),
            profile.as_deref(),
            connection.as_deref(),
            messaging.as_deref()
        ),
        (Some("bg-1"), Some("ovp-1"), Some("conn-1"), Some("mp-1"))
    );
    assert_ne!(app.as_deref(), Some("gone"));
    // The new app calls out through the existing voice profile.
    assert_eq!(posts[1]["outbound"]["outbound_voice_profile_id"], "ovp-1");
}

#[test_db::test(seed)]
async fn a_failed_connect_keeps_what_was_made(pool: MySqlPool) {
    let (server, stubs) = phone_server(pool.clone()).await;
    stubs.telnyx_down.store(true, Ordering::SeqCst);
    server
        .post(CONNECT)
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::BAD_GATEWAY);
    assert_eq!(ids(&pool, 100).await, [None, None, None, None, None]);
    stubs.telnyx_down.store(false, Ordering::SeqCst);
    let response = server.post(CONNECT).with_session(sessions::ADMIN).await;
    response.assert_status_ok();
    assert_eq!(
        response.json::<Value>()["created"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
}

#[test_db::test(seed)]
async fn superusers_connect_any_company(pool: MySqlPool) {
    sql(&pool, "UPDATE users SET is_superuser = 1 WHERE id = 103").await;
    let (server, _) = phone_server(pool.clone()).await;
    let response = server
        .post("/v1/superuser/companies/101/telnyx/connect")
        .with_session(sessions::SUPER_ADMIN)
        .await;
    response.assert_status_ok();
    let setup = response.json::<Value>();
    assert_eq!(setup["enabled"], true);
    assert_eq!(setup["webhookBase"], webhook_base(&pool, 101).await);
    assert!(setup["texmlAppId"].as_str().is_some());
    assert!(setup["credentialConnectionId"].as_str().is_some());
    assert!(setup["messagingProfileId"].as_str().is_some());
    server
        .post("/v1/superuser/companies/999/telnyx/connect")
        .with_session(sessions::SUPER_ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .post("/v1/superuser/companies/101/telnyx/connect")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn admins_search_and_buy_numbers(pool: MySqlPool) {
    let (server, stubs) = phone_server(pool.clone()).await;
    let search = server
        .get("/v1/phone/admin/numbers/available?areaCode=317")
        .with_session(sessions::ADMIN)
        .await;
    search.assert_status_ok();
    assert_eq!(
        search.json::<Value>(),
        json!({ "numbers": [
            { "e164": "+13175550188", "locality": "INDIANAPOLIS", "region": "IN",
              "monthlyCost": "1.00", "upfrontCost": "1.00" },
            { "e164": "+13175550189", "locality": null, "region": null,
              "monthlyCost": null, "upfrontCost": null },
        ] })
    );
    let query = stubs.telnyx_paths()[0].clone();
    for part in [
        "filter%5Bnational_destination_code%5D=317",
        "filter%5Bfeatures%5D%5B%5D=voice",
        "filter%5Bfeatures%5D%5B%5D=sms",
    ] {
        assert!(query.contains(part), "{query}");
    }
    for bad in ["31", "1170", "117", "abc"] {
        server
            .get(&format!("/v1/phone/admin/numbers/available?areaCode={bad}"))
            .with_session(sessions::ADMIN)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    server
        .get("/v1/phone/admin/numbers/available?areaCode=317")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);

    let buy = |e164: &str| {
        server
            .post("/v1/phone/admin/numbers/buy")
            .with_session(sessions::ADMIN)
            .json(&json!({ "e164": e164 }))
    };
    // Not connected yet.
    buy("+13175550188")
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .post(CONNECT)
        .with_session(sessions::ADMIN)
        .await
        .assert_status_ok();
    let [group, _, app, _, messaging] = ids(&pool, 100).await;
    stubs.clear_telnyx();

    let bought = buy("(317) 555-0188").await;
    bought.assert_status_ok();
    let number = bought.json::<Value>();
    assert_eq!(
        (
            &number["e164"],
            &number["kind"],
            &number["label"],
            &number["assignments"]
        ),
        (
            &json!("+13175550188"),
            &json!("direct"),
            &json!(""),
            &json!([])
        )
    );
    assert_eq!(
        sent(&stubs, "POST", "/number_orders"),
        json!({
            "phone_numbers": [{ "phone_number": "+13175550188" }],
            "connection_id": app, "messaging_profile_id": messaging,
            "billing_group_id": group, "customer_reference": "company-100",
        })
    );
    // Twice, or one another company has: not again.
    buy("+13175550188")
        .await
        .assert_status(StatusCode::CONFLICT);
    add_number(&pool, 9951, 101, "+13175550187", "direct", "").await;
    buy("+13175550187")
        .await
        .assert_status(StatusCode::CONFLICT);
    // Telnyx would not sell it.
    buy(REFUSED_NUMBER)
        .await
        .assert_status(StatusCode::BAD_GATEWAY);
    // Not a US or Canadian number.
    buy("+442071838750")
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .post("/v1/phone/admin/numbers/buy")
        .with_session(sessions::REP)
        .json(&json!({ "e164": "+13175550189" }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    // Not while Granite Manager has switched the company's calling off.
    sql(
        &pool,
        "UPDATE telnyx_company_config SET enabled = 0 WHERE company_id = 100",
    )
    .await;
    buy("+13175550189")
        .await
        .assert_status(StatusCode::FORBIDDEN);
    sql(
        &pool,
        "UPDATE telnyx_company_config SET enabled = 1 WHERE company_id = 100",
    )
    .await;
    // 25 numbers is the most.
    for n in 0..24 {
        add_number(
            &pool,
            9960 + n,
            100,
            &format!("+1317555{:04}", 3000 + n),
            "direct",
            "",
        )
        .await;
    }
    buy("+13175550189")
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let actions = audit_actions(&pool, 100).await;
    assert_eq!(
        actions
            .iter()
            .filter(|action| *action == "number_bought")
            .count(),
        1
    );
}
