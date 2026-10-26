//! Granite Phone (Telnyx) calls in the CRM, with the session cookie: the
//! call panels' list (`GET /v1/calls/telnyx`, deals, customers and phones),
//! recordings, transcripts, the bell (missed calls, voicemails, AI messages,
//! 911 alerts) and what an ended call sets off in a company that calls and
//! texts through nothing but Telnyx (Acme, company 100, has no CloudTalk or
//! RingCentral).
//!
//! Fixture numbers 9761+; calls 982000+; customers 932000+, deals 932100+.

use crate::common;
use crate::phone_common;

use api::app;
use api::phone::call_log::{self, Ended};
use api::phone::calls::CallStatus;
use api::phone::{PhoneConfig, outcomes};
use api::state::AppState;
use api::storage::Storage;
use axum::http::StatusCode;
use axum_test::TestServer;
use common::{WithSession, sessions};
use phone_common::{Stubs, add_call, add_number, assign, enable_calling, phone_state, sql};
use serde_json::{Value, json};
use sqlx::MySqlPool;

/// The API (Telnyx and AssemblyAI stand-ins, a recordings bucket the test
/// can fill) and its state.
async fn server(pool: &MySqlPool) -> (TestServer, Stubs, Storage, AppState) {
    let storage = Storage::memory();
    let (state, stubs) = phone_state(pool.clone()).await;
    let state = state.with_storage(storage.clone());
    (TestServer::new(app(state.clone())), stubs, storage, state)
}

/// Paula (company 100, two numbers) and her open deal of 2026-10-05 in the
/// first stage; a customer of company 101 with Paula's number. Rita's
/// direct line, the Sales line (rings Rita and Mark) and the main line.
async fn fixtures(pool: &MySqlPool) {
    enable_calling(pool, 100, "").await;
    sql(
        pool,
        "INSERT INTO customers (id, name, phone, phone_2, company_id, source, sales_rep) VALUES
           (932001, 'Paula Phone', '317-555-0161', '(317) 555-0162', 100, 'leads', 100),
           (932002, 'Other Co Caller', '317-555-0161', NULL, 101, 'leads', 106)",
    )
    .await;
    sql(
        pool,
        "INSERT INTO deals (id, customer_id, title, status, list_id, position, user_id, created_at) VALUES
           (932101, 932001, 'Paula kitchen', 'New', 100, 0, 100, '2026-10-05 08:00:00')",
    )
    .await;
    add_number(pool, 9761, 100, "+13175550171", "direct", "Rita direct").await;
    add_number(pool, 9762, 100, "+13175550172", "shared", "Sales").await;
    add_number(pool, 9763, 100, "+13175550170", "main", "").await;
    assign(pool, 9761, 100, true, true).await;
    assign(pool, 9762, 100, true, true).await;
    assign(pool, 9762, 101, true, true).await;
    assign(pool, 9763, 101, true, false).await;
}

/// The calls of the call-panel tests.
async fn calls(pool: &MySqlPool) {
    // Rita's two minutes with Paula, recorded, with its AI note on the deal.
    add_call(
        pool,
        982_001,
        100,
        Some(100),
        "outbound",
        "answered",
        "+13175550161",
        "2026-10-10 10:00:00",
        "customer_id = 932001, deal_id = 932101, number_id = 9761, our_number = '+13175550171',
         answered_at = '2026-10-10 10:00:05', ended_at = '2026-10-10 10:02:05', duration_secs = 120,
         recording_status = 'copied', recording_key = 'phone/100/2026/10/982001-call.mp3',
         transcript_status = 'done', ai_note = '- **Project:** kitchen', note_id = 1",
    )
    .await;
    // A voicemail from Paula's second number, before she was matched.
    add_call(pool, 982_002, 100, None, "inbound", "voicemail", "+13175550162", "2026-10-11 09:00:00",
        "number_id = 9762, our_number = '+13175550172', ended_at = '2026-10-11 09:01:00',
         recording_status = 'copied', recording_key = 'phone/100/2026/10/982002-voicemail.mp3',
         transcript_status = 'done', transcript = 'Speaker A: Please call me back about the quartz.'").await;
    // An unknown caller, and Paula's missed call before her deal.
    add_call(
        pool,
        982_003,
        100,
        None,
        "inbound",
        "missed",
        "+13175550199",
        "2026-10-12 09:00:00",
        "number_id = 9761, ended_at = '2026-10-12 09:00:30'",
    )
    .await;
    add_call(
        pool,
        982_004,
        100,
        None,
        "inbound",
        "missed",
        "+13175550161",
        "2026-10-04 12:00:00",
        "customer_id = 932001, number_id = 9761, ended_at = '2026-10-04 12:00:30'",
    )
    .await;
    // A test call, a live call with Paula, and company 101's call with her number.
    add_call(
        pool,
        982_005,
        100,
        Some(100),
        "inbound",
        "answered",
        "+13175550171",
        "2026-10-12 10:00:00",
        "number_id = 9761, our_number = '+13175550171', answered_at = '2026-10-12 10:00:02',
         ended_at = '2026-10-12 10:00:09', duration_secs = 7",
    )
    .await;
    add_call(
        pool,
        982_006,
        100,
        Some(100),
        "outbound",
        "in_progress",
        "+13175550161",
        "2026-10-12 11:00:00",
        "customer_id = 932001, answered_at = '2026-10-12 11:00:04'",
    )
    .await;
    add_call(pool, 982_007, 101, Some(106), "outbound", "answered", "+13175550161", "2026-10-12 12:00:00",
        "customer_id = 932002, answered_at = '2026-10-12 12:00:04', ended_at = '2026-10-12 12:01:04',
         duration_secs = 60").await;
    // Logged to Paula from a number she no longer has.
    add_call(pool, 982_008, 100, Some(101), "inbound", "answered", "+13175550188", "2026-10-12 08:00:00",
        "customer_id = 932001, deal_id = 932101, number_id = 9762, answered_at = '2026-10-12 08:00:03',
         ended_at = '2026-10-12 08:00:48', duration_secs = 45").await;
}

/// `GET /v1/calls/telnyx?{query}` as `session`: the items.
async fn list(server: &TestServer, session: &str, query: &str) -> Vec<Value> {
    let response = server
        .get(&format!("/v1/calls/telnyx?{query}"))
        .with_session(session)
        .await;
    response.assert_status_ok();
    response.json::<Value>()["items"]
        .as_array()
        .unwrap()
        .clone()
}

fn ids(items: &[Value]) -> Vec<&str> {
    items
        .iter()
        .map(|item| item["Cdr"]["id"].as_str().unwrap())
        .collect()
}

#[test_db::test(seed)]
async fn the_call_panels_list_granite_phone_calls(pool: MySqlPool) {
    fixtures(&pool).await;
    calls(&pool).await;
    let (server, _, _, _) = server(&pool).await;
    // The deal: calls with Paula's numbers, or logged to her, since the deal
    // was created; never test calls, live calls or another company's.
    let deal = list(&server, sessions::REP, "deal_id=932101").await;
    assert_eq!(ids(&deal), ["tx_982008", "tx_982002", "tx_982001"]);
    let rita = &deal[2];
    assert_eq!(
        rita["Cdr"],
        json!({
            "id": "tx_982001", "billsec": "120", "type": "outgoing",
            "public_external": "+13175550161", "public_internal": "+13175550171",
            "recorded": true, "is_voicemail": false, "fax_email": "", "is_redirected": "0",
            "redirected_from": "", "transferred_from": "", "is_local": false, "user_id": "100",
            "talking_time": "120", "started_at": "2026-10-10T10:00:00.000Z",
            "answered_at": "2026-10-10T10:00:05.000Z", "ended_at": "2026-10-10T10:02:05.000Z",
            "waiting_time": "0", "wrapup_time": "0", "recording_link": "tx_982001",
        })
    );
    assert_eq!(
        rita["Telnyx"],
        json!({
            "call_id": 982001, "status": "answered", "line_label": "Rita direct",
            "our_number": "+13175550171", "customer_id": 932001, "deal_id": 932101,
            "transcript_status": "done", "ai_note": "- **Project:** kitchen", "note_added": true,
            "via_cell": false, "spam_likely": false, "may_hear": true,
        })
    );
    assert_eq!(rita["Agent"]["id"], 100);
    assert_eq!(rita["Agent"]["firstname"], "Rita Rep");
    assert_eq!(rita["Contact"]["id"], 932001);
    assert_eq!(rita["Contact"]["name"], "Paula Phone");
    assert_eq!(rita["CallNumber"]["internal_name"], "Rita direct");
    let voicemail = &deal[1];
    assert_eq!(voicemail["Cdr"]["type"], "incoming");
    assert_eq!(voicemail["Cdr"]["is_voicemail"], true);
    assert_eq!(voicemail["Cdr"]["talking_time"], "0");
    assert_eq!(voicemail["Cdr"]["user_id"], "");
    assert_eq!(voicemail["Contact"]["id"], 0);
    assert_eq!(voicemail["Telnyx"]["status"], "voicemail");
    assert_eq!(voicemail["Telnyx"]["line_label"], "Sales");
    assert_eq!(voicemail["Telnyx"]["note_added"], false);
    // The customer's history (no date limit), and by phone.
    assert_eq!(
        ids(&list(&server, sessions::REP, "customer_id=932001").await),
        ["tx_982008", "tx_982002", "tx_982001", "tx_982004"]
    );
    assert_eq!(
        ids(&list(
            &server,
            sessions::MANAGER,
            "phone=317-555-0161&phone2=3175550199"
        )
        .await),
        ["tx_982003", "tx_982001", "tx_982004"]
    );
    assert_eq!(
        ids(&list(
            &server,
            sessions::REP,
            "phone=3175550161&date_from=2026-10-05&date_to=2026-10-10"
        )
        .await),
        ["tx_982001"]
    );
    // Another company's deal or customer: nothing; its own calls with the number only.
    assert!(
        list(&server, sessions::OTHER_COMPANY_REP, "deal_id=932101")
            .await
            .is_empty()
    );
    assert!(
        list(&server, sessions::OTHER_COMPANY_REP, "customer_id=932001")
            .await
            .is_empty()
    );
    assert_eq!(
        ids(&list(&server, sessions::OTHER_COMPANY_REP, "phone=3175550161").await),
        ["tx_982007"]
    );
    assert!(list(&server, sessions::REP, "").await.is_empty());
    server
        .get("/v1/calls/telnyx?deal_id=932101")
        .with_session(sessions::VIEWER)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .get("/v1/calls/telnyx?deal_id=932101")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    server
        .get("/v1/calls/vonage?deal_id=932101")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

#[test_db::test(seed)]
async fn recordings_play_from_our_bucket_for_the_company(pool: MySqlPool) {
    fixtures(&pool).await;
    calls(&pool).await;
    let (server, _, _, _) = server(&pool).await;
    let recording = |session: &'static str, media: &str| {
        server
            .get(&format!("/v1/calls/telnyx/{media}/recording"))
            .with_session(session)
    };
    for session in [sessions::REP, sessions::MANAGER, sessions::ADMIN] {
        let link: Value = recording(session, "tx_982001").await.json();
        assert_eq!(link["content_type"], "audio/mpeg");
        assert!(
            link["url"]
                .as_str()
                .unwrap()
                .ends_with("/phone/100/2026/10/982001-call.mp3"),
            "{link}"
        );
    }
    // Not recorded, another company's call, or not one of our ids.
    recording(sessions::REP, "tx_982004")
        .await
        .assert_status_not_found();
    recording(sessions::OTHER_COMPANY_REP, "tx_982001")
        .await
        .assert_status_not_found();
    for media in ["982001", "tx_abc", "rec_5"] {
        recording(sessions::REP, media)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    server
        .get("/v1/calls/cloudtalk/tx_982001/recording")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    recording(sessions::VIEWER, "tx_982001")
        .await
        .assert_status(StatusCode::FORBIDDEN);
    // Without a recordings bucket on the server.
    let (state, _) = phone_state(pool.clone()).await;
    let bare = TestServer::new(app(state.with_phone_config(PhoneConfig::default())));
    bare.get("/v1/calls/telnyx/tx_982001/recording")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::SERVICE_UNAVAILABLE);
}

#[test_db::test(seed)]
async fn transcripts_are_kept_on_the_call(pool: MySqlPool) {
    fixtures(&pool).await;
    calls(&pool).await;
    let (server, stubs, storage, _) = server(&pool).await;
    let transcribe = |session: &'static str, call: &str| {
        server
            .post("/v1/call-transcripts")
            .with_session(session)
            .json(&json!({ "call_id": call, "recording_link": call }))
    };
    // Kept already (the AI note's): no transcription.
    let kept: Value = transcribe(sessions::REP, "tx_982002").await.json();
    assert_eq!(
        kept,
        json!({ "text": "Speaker A: Please call me back about the quartz." })
    );
    assert!(stubs.uploads().is_empty());
    // Not kept yet: our copy is transcribed (speaker turns) and kept.
    storage
        .put_private(
            "phone-recordings",
            "phone/100/2026/10/982001-call.mp3",
            b"audio-982001".to_vec(),
            "audio/mpeg",
        )
        .await
        .unwrap();
    stubs.add_transcript(
        b"audio-982001",
        "Hi Paula. Hi Rita, the white quartz please.",
        &[
            ("A", "Hi Paula."),
            ("B", "Hi Rita, the white quartz please."),
        ],
    );
    let fresh: Value = transcribe(sessions::MANAGER, "tx_982001").await.json();
    let text = "Speaker A: Hi Paula.\nSpeaker B: Hi Rita, the white quartz please.";
    assert_eq!(fresh, json!({ "text": text }));
    assert_eq!(stubs.transcript_requests()[0]["speaker_labels"], true);
    let stored: Option<String> =
        sqlx::query_scalar("SELECT transcript FROM phone_calls WHERE id = 982001")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored.as_deref(), Some(text));
    transcribe(sessions::REP, "tx_982001")
        .await
        .assert_status_ok();
    assert_eq!(stubs.uploads().len(), 1);
    // Another company's call, or no recording: not found.
    transcribe(sessions::OTHER_COMPANY_REP, "tx_982001")
        .await
        .assert_status_not_found();
    transcribe(sessions::REP, "tx_982004")
        .await
        .assert_status_not_found();
}

#[test_db::test(seed)]
async fn the_panels_show_recordings_and_ai_notes_to_those_who_may_hear(pool: MySqlPool) {
    fixtures(&pool).await;
    calls(&pool).await;
    // Dina owns Paula's bathroom deal; Mark called Paula about it.
    sql(&pool, "INSERT INTO deals (id, customer_id, title, status, list_id, position, user_id, created_at) VALUES (932103, 932001, 'Paula bath', 'New', 100, 1, 107, '2026-10-05 08:00:00')").await;
    add_call(&pool, 982_009, 100, Some(101), "outbound", "answered", "+13175550161", "2026-10-12 13:00:00",
        "customer_id = 932001, deal_id = 932103, number_id = 9762, answered_at = '2026-10-12 13:00:03',
         ended_at = '2026-10-12 13:01:03', duration_secs = 60, recording_status = 'copied',
         recording_key = 'phone/100/2026/10/982009-call.mp3', transcript_status = 'done',
         ai_note = '- **Project:** bath', transcript = 'Speaker A: The bath vanity.'").await;
    let (server, stubs, _, _) = server(&pool).await;
    // Dina sees Rita's deal's calls, but not what was said in them.
    let calls = list(&server, sessions::DEALER, "deal_id=932101").await;
    assert_eq!(
        ids(&calls),
        ["tx_982009", "tx_982008", "tx_982002", "tx_982001"]
    );
    for call in &calls[1..] {
        assert_eq!(
            (
                &call["Cdr"]["recorded"],
                &call["Cdr"]["recording_link"],
                &call["Telnyx"]["ai_note"],
                &call["Telnyx"]["may_hear"]
            ),
            (&json!(false), &json!(""), &Value::Null, &json!(false)),
            "{call}"
        );
    }
    // Mark's call is on her deal: hers to hear.
    assert_eq!(calls[0]["Cdr"]["recording_link"], "tx_982009");
    assert_eq!(calls[0]["Telnyx"]["ai_note"], "- **Project:** bath");
    assert_eq!(calls[0]["Telnyx"]["may_hear"], true);
    let recording = |session: &'static str, media: &str| {
        server
            .get(&format!("/v1/calls/telnyx/{media}/recording"))
            .with_session(session)
    };
    recording(sessions::DEALER, "tx_982001")
        .await
        .assert_status(StatusCode::FORBIDDEN);
    recording(sessions::DEALER, "tx_982002")
        .await
        .assert_status(StatusCode::FORBIDDEN);
    recording(sessions::DEALER, "tx_982009")
        .await
        .assert_status_ok();
    // Rita: her line's voicemail and her deal's calls; the deal's owner,
    // the manager and the admin hear Mark's call too.
    recording(sessions::REP, "tx_982002")
        .await
        .assert_status_ok();
    for session in [sessions::MANAGER, sessions::ADMIN] {
        recording(session, "tx_982009").await.assert_status_ok();
    }
    recording(sessions::REP, "tx_982009")
        .await
        .assert_status(StatusCode::FORBIDDEN);
    // Transcripts: refused before anything is transcribed (by the call id
    // or the recording link that names the call).
    for body in [
        json!({ "call_id": "tx_982001", "recording_link": "tx_982001" }),
        json!({ "call_id": "777", "recording_link": "tx_982001" }),
    ] {
        server
            .post("/v1/call-transcripts")
            .with_session(sessions::DEALER)
            .json(&body)
            .await
            .assert_status(StatusCode::FORBIDDEN);
    }
    assert!(stubs.uploads().is_empty());
    assert!(stubs.transcript_requests().is_empty());
    let kept: Value = server
        .post("/v1/call-transcripts")
        .with_session(sessions::DEALER)
        .json(&json!({ "call_id": "tx_982009", "recording_link": "tx_982009" }))
        .await
        .json();
    assert_eq!(kept, json!({ "text": "Speaker A: The bath vanity." }));
}

/// The bell's items of `session`: (id, title, message, type title, href).
async fn bell(server: &TestServer, session: &str) -> Vec<(String, String, String, String, String)> {
    let body: Value = server
        .get("/v1/notifications")
        .with_session(session)
        .await
        .json();
    body["notifications"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| {
            let id = item["id"].as_str().unwrap();
            id.starts_with("telnyx-call-") || item["notification_type"] == "e911_alert"
        })
        .map(|item| {
            let text = |key: &str| item[key].as_str().unwrap_or_default().to_string();
            (
                text("id"),
                text("title"),
                text("message"),
                text("type_title"),
                text("href"),
            )
        })
        .collect()
}

#[test_db::test(seed)]
async fn the_bell_lists_unseen_missed_calls_voicemails_and_ai_messages(pool: MySqlPool) {
    fixtures(&pool).await;
    let ended = |hours: i64| format!("ended_at = UTC_TIMESTAMP(3) - INTERVAL {hours} HOUR");
    let add = |id: i64,
               status: &'static str,
               number: i32,
               from: &'static str,
               hours: i64,
               extra: String| {
        let pool = pool.clone();
        async move {
            add_call(
                &pool,
                id,
                100,
                None,
                "inbound",
                status,
                from,
                "2026-01-01 00:00:00",
                &format!("number_id = {number}, {extra}"),
            )
            .await;
            sql(&pool, &format!("UPDATE phone_calls SET started_at = UTC_TIMESTAMP(3) - INTERVAL {hours} HOUR WHERE id = {id}")).await;
        }
    };
    add(982_101, "missed", 9761, "+13175550199", 1, ended(1)).await;
    add(
        982_102,
        "voicemail",
        9762,
        "+13175550161",
        2,
        format!("customer_id = 932001, deal_id = 932101, {}", ended(2)),
    )
    .await;
    add(982_103, "ai_message", 9763, "+13175550161", 3, ended(3)).await;
    add(982_104, "missed", 9761, "+13175550199", 100, ended(100)).await;
    add(
        982_105,
        "answered",
        9761,
        "+13175550199",
        4,
        format!(
            "user_id = 100, answered_at = UTC_TIMESTAMP(3) - INTERVAL 4 HOUR, {}",
            ended(4)
        ),
    )
    .await;
    add(982_106, "missed", 9761, "+13175550199", 5, ended(5)).await;
    add(
        982_107,
        "ai_message",
        9762,
        "+13175550162",
        6,
        format!("customer_id = 932001, {}", ended(6)),
    )
    .await;
    sql(&pool, "INSERT INTO phone_call_views (call_id, user_id, seen_at) VALUES (982106, 100, UTC_TIMESTAMP())").await;
    let (server, _, _, _) = server(&pool).await;
    let item = |id: &str, title: &str, message: &str, kind: &str, href: &str| {
        (
            id.to_string(),
            title.to_string(),
            message.to_string(),
            kind.to_string(),
            href.to_string(),
        )
    };
    let missed = item(
        "telnyx-call-982101",
        "+13175550199",
        "Missed call",
        "Missed call · Rita direct",
        "/employee/customers",
    );
    let voicemail = item(
        "telnyx-call-982102",
        "Paula Phone",
        "New voicemail",
        "Voicemail · Sales",
        "/employee/deals/edit/932101/project",
    );
    let message = item(
        "telnyx-call-982107",
        "Paula Phone",
        "New AI message",
        "AI message · Sales",
        "/employee/customers/info/932001/info",
    );
    // Rita: her direct line and the Sales line; not the main line (it does
    // not ring her), old, answered or seen calls.
    assert_eq!(
        bell(&server, sessions::REP).await,
        [missed.clone(), voicemail.clone(), message.clone()]
    );
    // Mark: the Sales line only; the main line lists no label.
    assert_eq!(
        bell(&server, sessions::MANAGER).await,
        [
            voicemail.clone(),
            item(
                "telnyx-call-982103",
                "+13175550161",
                "New AI message",
                "AI message",
                "/employee/customers"
            ),
            message.clone(),
        ]
    );
    assert!(bell(&server, sessions::OTHER_COMPANY_REP).await.is_empty());
    // Dismissing one marks it seen for Rita only (as the app's Recents).
    server
        .post("/v1/notifications/done")
        .with_session(sessions::REP)
        .json(
            &json!({ "items": [{ "id": "telnyx-call-982101", "kind": "call" },
                                 { "id": "telnyx-call-982007", "kind": "call" }] }),
        )
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        bell(&server, sessions::REP).await,
        [voicemail.clone(), message.clone()]
    );
    let seen: Vec<(i64, i32)> = sqlx::query_as(
        "SELECT CAST(call_id AS SIGNED), user_id FROM phone_call_views WHERE seen_at IS NOT NULL ORDER BY call_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(seen, [(982_101, 100), (982_106, 100)]);
    // Clearing the bell marks the rest seen.
    server
        .post("/v1/notifications/done")
        .with_session(sessions::REP)
        .json(&json!({ "clear_all": true, "hide_only": true }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert!(bell(&server, sessions::REP).await.is_empty());
    assert_eq!(bell(&server, sessions::MANAGER).await.len(), 3);
}

#[test_db::test(seed)]
async fn the_bell_shows_911_alerts_to_the_admins(pool: MySqlPool) {
    fixtures(&pool).await;
    let (server, _, _, state) = server(&pool).await;
    let alerted = api::phone::alerts::alert_e911(
        &state,
        100,
        api::phone::alerts::AlertSource::Company,
        Some(100),
        "Rita Rep",
        "911",
    )
    .await
    .unwrap();
    assert!(alerted);
    let alerts = bell(&server, sessions::ADMIN).await;
    assert_eq!(alerts.len(), 1);
    let (id, title, message, kind, href) = alerts[0].clone();
    assert!(id.starts_with("notif-"));
    assert_eq!(
        (
            title.as_str(),
            message.as_str(),
            kind.as_str(),
            href.as_str()
        ),
        (
            "Emergency call",
            "Rita Rep dialed 911 on Granite Phone",
            "Emergency Call",
            "/admin/phone"
        )
    );
    // The caller is not alerted.
    assert!(bell(&server, sessions::REP).await.is_empty());
    server
        .post("/v1/notifications/done")
        .with_session(sessions::ADMIN)
        .json(&json!({ "items": [{ "id": id, "kind": "activity" }] }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert!(bell(&server, sessions::ADMIN).await.is_empty());
}

/// Flow runs of company 100: Paula's by her number, by her, by her other
/// number, and another customer's.
async fn flows(pool: &MySqlPool) {
    sql(pool, "INSERT INTO sms_flows (id, company_id, user_id, name) VALUES (9760, 100, 100, 'Rita flow')").await;
    sql(
        pool,
        "INSERT INTO sms_flow_enrollments (id, flow_id, company_id, customer_phone_digits, customer_id, user_id,
                                           status, current_step_position, anchor_at, next_send_at) VALUES
           (9861, 9760, 100, 3175550161, NULL, 100, 'active', 1, '2026-10-01 12:00:00', '2026-10-30 12:00:00'),
           (9862, 9760, 100, NULL, 932001, 100, 'paused', 1, '2026-10-01 12:00:00', '2026-10-30 12:00:00'),
           (9863, 9760, 100, 3175550162, NULL, 100, 'active', 1, '2026-10-01 12:00:00', '2026-10-30 12:00:00'),
           (9864, 9760, 100, 3175550199, NULL, 100, 'active', 1, '2026-10-01 12:00:00', '2026-10-30 12:00:00')",
    )
    .await;
}

async fn flow_statuses(pool: &MySqlPool) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT CAST(status AS CHAR) FROM sms_flow_enrollments WHERE id BETWEEN 9861 AND 9864 ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn deal_stage(pool: &MySqlPool) -> i32 {
    sqlx::query_scalar("SELECT list_id FROM deals WHERE id = 932101")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Ends fixture call `id` (as it is now) with `status` after `talk_secs`.
async fn end(state: &AppState, id: i64, status: CallStatus, talk_secs: Option<i32>) {
    let before = call_log::facts(&state.pool, 100, id)
        .await
        .unwrap()
        .unwrap();
    outcomes::after_end(
        state,
        &Ended {
            before,
            status,
            talk_secs,
        },
    )
    .await;
}

#[test_db::test(seed)]
async fn a_customer_calling_in_moves_the_deal_and_stops_the_flows(pool: MySqlPool) {
    fixtures(&pool).await;
    flows(&pool).await;
    let (_, _, _, state) = server(&pool).await;
    // A Telnyx-only company: no SMS provider, the flows stop anyway.
    add_call(
        &pool,
        982_201,
        100,
        None,
        "inbound",
        "ringing",
        "+13175550161",
        "2026-10-12 09:00:00",
        "customer_id = 932001, deal_id = 932101, number_id = 9761",
    )
    .await;
    end(&state, 982_201, CallStatus::Canceled, None).await;
    assert_eq!(
        flow_statuses(&pool).await,
        [
            "stopped_by_reply",
            "stopped_by_reply",
            "stopped_by_reply",
            "active"
        ]
    );
    // Like CloudTalk's webhook: any call from the customer moves the deal on.
    assert_eq!(deal_stage(&pool).await, 101);
    let stages: Vec<(i32, bool)> = sqlx::query_as(
        "SELECT list_id, exited_at IS NULL FROM deal_stage_history WHERE deal_id = 932101 ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(stages, [(101, true)]);
}

#[test_db::test(seed)]
async fn a_rep_call_counts_once_it_reached_the_customer(pool: MySqlPool) {
    fixtures(&pool).await;
    flows(&pool).await;
    let (_, _, _, state) = server(&pool).await;
    // 45 seconds: nothing yet (its transcript decides, like CloudTalk's check).
    add_call(
        &pool,
        982_301,
        100,
        Some(100),
        "outbound",
        "in_progress",
        "+13175550161",
        "2026-10-12 09:00:00",
        "customer_id = 932001, deal_id = 932101, answered_at = '2026-10-12 09:00:05'",
    )
    .await;
    end(&state, 982_301, CallStatus::Answered, Some(45)).await;
    assert_eq!(
        flow_statuses(&pool).await,
        ["active", "paused", "active", "active"]
    );
    assert_eq!(deal_stage(&pool).await, 100);
    // Two minutes: the deal moves and the flows stop.
    add_call(
        &pool,
        982_302,
        100,
        Some(100),
        "outbound",
        "in_progress",
        "+13175550161",
        "2026-10-12 10:00:00",
        "customer_id = 932001, deal_id = 932101, answered_at = '2026-10-12 10:00:05'",
    )
    .await;
    end(&state, 982_302, CallStatus::Answered, Some(120)).await;
    assert_eq!(
        flow_statuses(&pool).await,
        [
            "stopped_by_reply",
            "stopped_by_reply",
            "stopped_by_reply",
            "active"
        ]
    );
    assert_eq!(deal_stage(&pool).await, 101);
    // An unanswered call we placed changes nothing.
    sql(&pool, "UPDATE deals SET list_id = 100 WHERE id = 932101").await;
    sql(
        &pool,
        "UPDATE sms_flow_enrollments SET status = 'active' WHERE id BETWEEN 9861 AND 9863",
    )
    .await;
    add_call(
        &pool,
        982_303,
        100,
        Some(100),
        "outbound",
        "ringing",
        "+13175550161",
        "2026-10-12 11:00:00",
        "customer_id = 932001, deal_id = 932101",
    )
    .await;
    end(&state, 982_303, CallStatus::NoAnswer, None).await;
    assert_eq!(
        flow_statuses(&pool).await,
        ["active", "active", "active", "active"]
    );
    assert_eq!(deal_stage(&pool).await, 100);
}

#[test_db::test(seed)]
async fn the_users_activity_feed_has_the_reps_granite_phone_calls(pool: MySqlPool) {
    fixtures(&pool).await;
    calls(&pool).await;
    // Recent: call n started (982100 - n) hours ago.
    sql(&pool, "UPDATE phone_calls SET started_at = UTC_TIMESTAMP(3) - INTERVAL (982100 - id) HOUR WHERE id BETWEEN 982001 AND 982008").await;
    let (server, _, _, _) = server(&pool).await;
    let body: Value = server
        .get("/v1/users/call-activity?user_ids=100,101,106")
        .with_session(sessions::ADMIN)
        .await
        .json();
    let rows = body.as_array().unwrap();
    let listed: Vec<(i64, &str, i64, &str)> = rows
        .iter()
        .map(|row| {
            (
                row["id"].as_i64().unwrap(),
                row["source"].as_str().unwrap(),
                row["user_id"].as_i64().unwrap(),
                row["action"].as_str().unwrap(),
            )
        })
        .collect();
    // Mark's answered Sales call; Rita's direct line's missed calls (owner
    // Rita) and her call. Never the shared line's unanswered voicemail, test
    // or live calls, or another company's.
    assert_eq!(
        listed,
        [
            (982_008, "telnyx_call", 101, "Received a call"),
            (982_004, "telnyx_call", 100, "Missed a call"),
            (982_003, "telnyx_call", 100, "Missed a call"),
            (982_001, "telnyx_call", 100, "Made a call"),
        ]
    );
    let rita = &rows[3];
    assert_eq!(rita["summary"], "Outgoing · 2m");
    assert_eq!(
        rita["tags"],
        json!(["Outgoing", "2m", "Recorded", "Rita direct"])
    );
    assert_eq!(rita["recording_link"], "tx_982001");
    assert_eq!(rita["body"], "- **Project:** kitchen");
    assert_eq!(rita["customer_name"], "Paula Phone");
    assert_eq!(rita["contact"], "+13175550161");
    assert_eq!(rita["talk_seconds"], 120);
    assert_eq!(rita["user_name"], "Rita Rep");
    assert_eq!(rows[1]["summary"], "Incoming · Missed");
    assert_eq!(rows[1]["recording_link"], Value::Null);
    assert_eq!(rows[0]["summary"], "Incoming · 45s");
}

#[test_db::test(seed)]
async fn the_call_coach_scores_granite_phone_calls(pool: MySqlPool) {
    fixtures(&pool).await;
    calls(&pool).await;
    sql(&pool, "UPDATE phone_calls SET started_at = UTC_TIMESTAMP(3) - INTERVAL (982100 - id) HOUR WHERE id BETWEEN 982001 AND 982008").await;
    sql(&pool, "UPDATE phone_calls SET transcript = 'Speaker A: Hi Paula, what is not working in your kitchen?\nSpeaker B: The old laminate.' WHERE id = 982001").await;
    let (server, stubs, _, _) = server(&pool).await;
    stubs.set_chat_reply(
        r#"{"relevant":true,"skip_reason":null,"score":77,"advice":"Ask about the timeline.","summary":"Quartz kitchen"}"#,
    );
    let coaching = |session: &'static str| server.get("/v1/me/call-coaching").with_session(session);
    // Rita's answered call of over a minute (not Mark's 45 seconds).
    let listed: Value = coaching(sessions::REP).await.json();
    assert_eq!(listed["eligible"], true);
    let calls = listed["calls"].as_array().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["call_id"], "tx_982001");
    assert_eq!(calls[0]["status"], "idle");
    assert_eq!(calls[0]["call_type"], "outgoing");
    assert_eq!(calls[0]["phone_display"], "317-555-0161");
    let scored: Value = server
        .post("/v1/me/call-coaching")
        .with_session(sessions::REP)
        .json(&json!({ "call_id": "tx_982001" }))
        .await
        .json();
    assert_eq!(scored["calls"][0]["status"], "ready");
    assert_eq!(scored["calls"][0]["score"], 77);
    assert_eq!(scored["calls"][0]["customer_name"], "Paula Phone");
    let prompt = stubs.chats()[0]["messages"][1]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        prompt.contains("Transcript:\nSpeaker A: Hi Paula"),
        "{prompt}"
    );
    // The stored transcript was used: nothing went to AssemblyAI.
    assert!(stubs.uploads().is_empty());
    let mark: Value = coaching(sessions::MANAGER).await.json();
    assert_eq!(mark["eligible"], true);
    assert_eq!(mark["calls"], json!([]));
    // Company 101 has no Granite Phone (and Olga no CloudTalk line).
    let olga: Value = coaching(sessions::OTHER_COMPANY_REP).await.json();
    assert_eq!(olga["eligible"], false);
}

#[test_db::test(seed)]
async fn a_short_rep_call_counts_once_its_transcript_shows_a_conversation(pool: MySqlPool) {
    fixtures(&pool).await;
    flows(&pool).await;
    sql(&pool, "INSERT INTO customers (id, name, phone, company_id, source, sales_rep) VALUES (932003, 'Quinn Quiet', '317-555-0163', 100, 'leads', 100)").await;
    sql(&pool, "INSERT INTO deals (id, customer_id, title, status, list_id, position, user_id) VALUES (932102, 932003, 'Quinn vanity', 'New', 100, 1, 100)").await;
    sql(&pool, "INSERT INTO sms_flow_enrollments (id, flow_id, company_id, customer_phone_digits, customer_id, user_id, status, current_step_position, anchor_at, next_send_at) VALUES (9865, 9760, 100, 3175550163, 932003, 100, 'active', 1, '2026-10-01 12:00:00', '2026-10-30 12:00:00')").await;
    // 45 seconds each, transcribed: a back-and-forth with Paula, a mailbox for Quinn.
    let talk = "answered_at = UTC_TIMESTAMP(3) - INTERVAL 2 HOUR, ended_at = UTC_TIMESTAMP(3) - INTERVAL 1 HOUR,
                duration_secs = 45, recording_status = 'copied', transcript_status = 'pending'";
    add_call(&pool, 982_401, 100, Some(100), "outbound", "answered", "+13175550161", "2026-10-12 09:00:00",
        &format!("customer_id = 932001, deal_id = 932101, recording_key = 'phone/100/2026/10/982401-call.mp3', {talk},
                  transcript = 'Speaker A: Hello this is Rita from Acme Stone\nSpeaker B: Oh hi, I am not available Tuesday though, sorry\nSpeaker A: No problem, how about Thursday morning then\nSpeaker B: Thursday works great for me, thank you so much'")).await;
    add_call(&pool, 982_402, 100, Some(100), "outbound", "answered", "+13175550163", "2026-10-12 10:00:00",
        &format!("customer_id = 932003, deal_id = 932102, recording_key = 'phone/100/2026/10/982402-call.mp3', {talk},
                  transcript = 'Speaker A: You have reached the voicemail of John, please leave a message\nSpeaker B: Hi John this is Rita from Acme Stone calling about your quote'")).await;
    let (server, stubs, _, _) = server(&pool).await;
    stubs.set_chat_reply("- **Project:** vanity");
    server
        .post("/v1/jobs/phone")
        .add_header("authorization", phone_common::JOB_KEY)
        .await
        .assert_status_ok();
    // Paula talked: her flows stop and her deal moves on. Quinn's mailbox does nothing.
    assert_eq!(
        flow_statuses(&pool).await,
        [
            "stopped_by_reply",
            "stopped_by_reply",
            "stopped_by_reply",
            "active"
        ]
    );
    assert_eq!(deal_stage(&pool).await, 101);
    let quinn: (String, i32) = sqlx::query_as(
        "SELECT CAST(e.status AS CHAR), d.list_id FROM sms_flow_enrollments e JOIN deals d ON d.id = 932102 WHERE e.id = 9865",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(quinn, ("active".to_string(), 100));
    // The kept transcripts were used: nothing went to AssemblyAI, and no
    // CloudTalk call points were written for our calls.
    assert!(stubs.uploads().is_empty());
    let cloudtalk_points: i64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS SIGNED) FROM phone_call_points WHERE provider = 'cloudtalk' AND provider_call_id LIKE 'tx_%'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cloudtalk_points, 0);
}
