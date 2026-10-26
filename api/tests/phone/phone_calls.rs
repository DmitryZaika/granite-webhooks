//! Granite Phone calls: Recents and its rules, one call, seen and hidden,
//! stats, recordings, notes, Call via my cell, test calls, declines and 911
//! alerts (`routes/phone/calls.rs`, `support.rs`).
//!
//! Fixture phones use n = 81..=110; numbers 9740+; calls 980300+.

use crate::common;
use crate::phone_common;

use api::phone::clock::{day_start, month_start, time_zone, week_start};
use axum::http::{Method, StatusCode};
use chrono::{DateTime, Duration, Utc};
use common::{WithSession, sessions};
use phone_common::{
    GONE_PUSH, HEADER, add_call, add_number, add_phone, add_session, assert_device_only, assign,
    enable_calling, phone_server, phone_server_with, revoke_session, set_settings, sql, token,
    webhook_base,
};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::atomic::Ordering;

/// Rita's direct line, the sales line (rings Rita and Mark), the main line
/// (does not ring Rita), Mark's direct line, and company 100's direct line
/// wrongly assigned to Olga (company 101).
async fn lines(pool: &MySqlPool) {
    add_number(pool, 9741, 100, "+13175550151", "direct", "Rita direct").await;
    add_number(pool, 9742, 100, "+13175550152", "shared", "Sales").await;
    add_number(pool, 9743, 100, "+13175550150", "main", "Main line").await;
    add_number(pool, 9744, 100, "+13175550154", "direct", "Mark direct").await;
    assign(pool, 9741, 100, true, true).await;
    assign(pool, 9742, 100, true, true).await;
    assign(pool, 9742, 101, true, true).await;
    assign(pool, 9743, 100, false, false).await;
    assign(pool, 9744, 101, true, true).await;
    assign(pool, 9741, 106, true, true).await;
}

/// The calls of the Recents tests.
async fn calls(pool: &MySqlPool) {
    add_call(
        pool,
        980301,
        100,
        Some(100),
        "outbound",
        "answered",
        "+13175550100",
        "2026-10-10 10:00:00",
        "customer_id = 1000, deal_id = 8007, number_id = 9741, our_number = '+13175550151',
         answered_at = '2026-10-10 10:00:05', ended_at = '2026-10-10 10:02:05', duration_secs = 120,
         recording_status = 'copied', recording_key = 'phone/100/2026/10/980301-call.mp3',
         transcript_status = 'done', ai_note = '- **Project:** kitchen'",
    )
    .await;
    // To Rita's line, answered by Mark (who handled it).
    add_call(
        pool,
        980302,
        100,
        Some(101),
        "inbound",
        "answered",
        "+13175550199",
        "2026-10-10 11:00:00",
        "number_id = 9741, answered_at = '2026-10-10 11:00:04', recording_status = 'copied',
         recording_key = 'phone/100/2026/10/980302-call.wav'",
    )
    .await;
    add_call(
        pool,
        980303,
        100,
        None,
        "inbound",
        "missed",
        "+13175550199",
        "2026-10-11 09:00:00",
        "number_id = 9742",
    )
    .await;
    add_call(
        pool,
        980304,
        100,
        Some(101),
        "inbound",
        "answered",
        "+13175550198",
        "2026-10-11 10:00:00",
        "number_id = 9742, answered_at = '2026-10-11 10:00:03'",
    )
    .await;
    add_call(
        pool,
        980305,
        100,
        None,
        "inbound",
        "missed",
        "+13175550198",
        "2026-10-11 11:00:00",
        "number_id = 9743",
    )
    .await;
    add_call(
        pool,
        980306,
        100,
        None,
        "inbound",
        "missed",
        "+13175550198",
        "2026-10-11 12:00:00",
        "number_id = 9741",
    )
    .await;
    sql(pool, "INSERT INTO phone_call_views (call_id, user_id, hidden_at) VALUES (980306, 100, UTC_TIMESTAMP())").await;
    // A test call: our number calling our own phone.
    add_call(pool, 980307, 100, Some(100), "inbound", "answered", "+13175550151", "2026-10-11 13:00:00", "number_id = 9741, our_number = '+13175550151', answered_at = '2026-10-11 13:00:02', duration_secs = 5").await;
    add_call(
        pool,
        980308,
        101,
        Some(106),
        "inbound",
        "missed",
        "+13175550199",
        "2026-10-11 14:00:00",
        "number_id = 9741",
    )
    .await;
    add_call(
        pool,
        980309,
        100,
        None,
        "inbound",
        "voicemail",
        "+13175550197",
        "2026-10-12 09:00:00",
        "number_id = 9741, spam_likely = 1",
    )
    .await;
    add_call(
        pool,
        980310,
        100,
        Some(100),
        "outbound",
        "no_answer",
        "+13175550102",
        "2026-10-12 10:00:00",
        "customer_id = 1001, number_id = 9741, our_number = '+13175550151', via_cell = 1",
    )
    .await;
    add_call(
        pool,
        980311,
        100,
        None,
        "inbound",
        "missed",
        "+13175550196",
        "2026-10-12 11:00:00",
        "number_id = 9744",
    )
    .await;
}

async fn recents(server: &axum_test::TestServer, n: u32, query: &str) -> Value {
    let response = server
        .get(&format!("/v1/phone/calls{query}"))
        .add_header(HEADER, token(n))
        .await;
    response.assert_status_ok();
    response.json()
}

fn ids(list: &Value) -> Vec<i64> {
    list["calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|call| call["id"].as_i64().unwrap())
        .collect()
}

#[test_db::test(seed)]
async fn recents_are_my_calls_and_my_lines(pool: MySqlPool) {
    lines(&pool).await;
    calls(&pool).await;
    add_session(&pool, 81, 100, 100).await;
    add_session(&pool, 82, 101, 100).await;
    add_session(&pool, 83, 106, 101).await;
    let (server, _) = phone_server(pool.clone()).await;
    let rita = recents(&server, 81, "").await;
    assert_eq!(ids(&rita), [980_310, 980_309, 980_303, 980_302, 980_301]);
    assert_eq!(rita["nextBefore"], Value::Null);
    assert_eq!(
        rita["calls"][4],
        json!({ "id": 980_301, "direction": "outbound", "status": "answered",
                "startedAt": "2026-10-10T10:00:00.000Z", "answeredAt": "2026-10-10T10:00:05.000Z",
                "endedAt": "2026-10-10T10:02:05.000Z", "durationSecs": 120,
                "otherNumber": "+13175550100",
                "customer": { "id": 1000, "name": "Alice Lead", "company": null },
                "dealId": 8007, "ourNumber": { "e164": "+13175550151", "label": "Rita direct" },
                "userId": 100, "userName": "Rita Rep", "hasRecording": true,
                "transcriptStatus": "done", "aiNote": "- **Project:** kitchen", "seen": true,
                "spamLikely": false, "viaCell": false })
    );
    assert_eq!(
        rita["calls"][2],
        json!({ "id": 980_303, "direction": "inbound", "status": "missed",
                "startedAt": "2026-10-11T09:00:00.000Z", "answeredAt": null, "endedAt": null,
                "durationSecs": null, "otherNumber": "+13175550199", "customer": null,
                "dealId": null, "ourNumber": { "e164": "+13175550152", "label": "Sales" },
                "userId": null, "userName": null, "hasRecording": false,
                "transcriptStatus": "none", "aiNote": null, "seen": false,
                "spamLikely": false, "viaCell": false })
    );
    assert_eq!(
        (
            &rita["calls"][0]["viaCell"],
            &rita["calls"][1]["spamLikely"],
            &rita["calls"][3]["seen"]
        ),
        (&json!(true), &json!(true), &json!(false))
    );
    // Mark: what he handled, his line, the sales line's unanswered call.
    assert_eq!(
        ids(&recents(&server, 82, "").await),
        [980_311, 980_304, 980_303, 980_302]
    );
    // Olga: only company 101's, whatever the assignments say.
    assert_eq!(ids(&recents(&server, 83, "").await), [980_308]);

    // Filters and one number's calls.
    for (query, expected) in [
        ("?filter=missed", vec![980_309, 980_303]),
        ("?filter=incoming", vec![980_302]),
        ("?filter=outgoing", vec![980_310, 980_301]),
        ("?filter=recorded", vec![980_302, 980_301]),
        ("?filter=all&number=%2B13175550100", vec![980_301]),
        ("?number=(317)%20555-0199", vec![980_303, 980_302]),
        ("?filter=missed&number=3175550199", vec![980_303]),
    ] {
        assert_eq!(ids(&recents(&server, 81, query).await), expected, "{query}");
    }
    // Pages.
    let first = recents(&server, 81, "?limit=2").await;
    assert_eq!(
        (ids(&first), &first["nextBefore"]),
        (vec![980_310, 980_309], &json!(980_309))
    );
    let second = recents(&server, 81, "?limit=2&before=980309").await;
    assert_eq!(
        (ids(&second), &second["nextBefore"]),
        (vec![980_303, 980_302], &json!(980_302))
    );
    let last = recents(&server, 81, "?limit=2&before=980302").await;
    assert_eq!(
        (ids(&last), &last["nextBefore"]),
        (vec![980_301], &Value::Null)
    );
    for bad in [
        "?limit=0",
        "?limit=101",
        "?filter=spam",
        "?number=12",
        "?before=x",
    ] {
        server
            .get(&format!("/v1/phone/calls{bad}"))
            .add_header(HEADER, token(81))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
}

#[test_db::test(seed)]
async fn seen_and_hidden_are_each_users_own(pool: MySqlPool) {
    lines(&pool).await;
    calls(&pool).await;
    add_session(&pool, 84, 100, 100).await;
    add_session(&pool, 85, 101, 100).await;
    let (server, _) = phone_server(pool.clone()).await;
    let seen = |body: Value| {
        server
            .post("/v1/phone/calls/seen")
            .add_header(HEADER, token(84))
            .json(&body)
    };
    seen(json!({ "ids": [980_303, 980_308, 999_999] }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let rita = recents(&server, 84, "").await;
    assert_eq!(rita["calls"][2]["id"], 980_303);
    assert_eq!(rita["calls"][2]["seen"], true);
    // Mark has not seen it.
    let mark = recents(&server, 85, "").await;
    assert_eq!(mark["calls"][2]["id"], 980_303);
    assert_eq!(mark["calls"][2]["seen"], false);
    // Everything in my Recents up to a time.
    seen(json!({ "upTo": "2026-10-11T00:00:00Z" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let views: Vec<(i64, bool, bool)> = sqlx::query_as(
        "SELECT call_id, seen_at IS NOT NULL, hidden_at IS NOT NULL FROM phone_call_views
          WHERE user_id = 100 ORDER BY call_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        views,
        [
            (980_301, true, false),
            (980_302, true, false),
            (980_303, true, false),
            (980_306, false, true)
        ]
    );
    assert_eq!(recents(&server, 84, "").await["calls"][3]["seen"], true);
    for bad in [
        json!({}),
        json!({ "ids": [1], "upTo": "2026-10-11T00:00:00Z" }),
        json!({ "ids": (0..501).collect::<Vec<_>>() }),
        json!({ "upTo": "yesterday" }),
    ] {
        seen(bad).await.assert_status(StatusCode::BAD_REQUEST);
    }
    seen(json!({ "ids": [] }))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    // Hiding is mine only, and can be done twice.
    for _ in 0..2 {
        server
            .delete("/v1/phone/calls/980303")
            .add_header(HEADER, token(84))
            .await
            .assert_status(StatusCode::NO_CONTENT);
    }
    assert_eq!(
        ids(&recents(&server, 84, "").await),
        [980_310, 980_309, 980_302, 980_301]
    );
    assert!(ids(&recents(&server, 85, "").await).contains(&980_303));
    for id in [980_308, 999_999] {
        server
            .delete(&format!("/v1/phone/calls/{id}"))
            .add_header(HEADER, token(84))
            .await
            .assert_status_not_found();
    }
}

#[test_db::test(seed)]
async fn a_call_is_read_by_id_in_the_company(pool: MySqlPool) {
    lines(&pool).await;
    calls(&pool).await;
    add_session(&pool, 86, 100, 100).await;
    let (server, _) = phone_server(pool.clone()).await;
    let get = |id: i64| {
        server
            .get(&format!("/v1/phone/calls/{id}"))
            .add_header(HEADER, token(86))
    };
    let voicemail = get(980_309).await;
    voicemail.assert_status_ok();
    assert_eq!(
        voicemail.json::<Value>(),
        json!({ "id": 980_309, "direction": "inbound", "status": "voicemail",
                "startedAt": "2026-10-12T09:00:00.000Z", "answeredAt": null, "endedAt": null,
                "durationSecs": null, "otherNumber": "+13175550197", "customer": null,
                "dealId": null, "ourNumber": { "e164": "+13175550151", "label": "Rita direct" },
                "userId": null, "userName": null, "hasRecording": false,
                "transcriptStatus": "none", "aiNote": null, "seen": false,
                "spamLikely": true, "viaCell": false })
    );
    // A company call outside my Recents (Mark's), and a hidden one: still
    // readable.
    assert_eq!(
        get(980_304).await.json::<Value>()["userName"],
        "Mark Manager"
    );
    get(980_306).await.assert_status_ok();
    get(980_308).await.assert_status_not_found();
    get(999_999).await.assert_status_not_found();
}

/// Rita's answered call `id`, `seconds` long, at `at`.
async fn talk(
    pool: &MySqlPool,
    id: i64,
    user_id: i32,
    at: DateTime<Utc>,
    seconds: Option<i32>,
    extra: &str,
) {
    let started = at.format("%Y-%m-%d %H:%M:%S").to_string();
    let duration = seconds.map_or_else(|| "NULL".to_string(), |s| s.to_string());
    let extra = if extra.is_empty() {
        String::new()
    } else {
        format!(", {extra}")
    };
    add_call(
        pool,
        id,
        100,
        Some(user_id),
        "outbound",
        "answered",
        "+13175550100",
        &started,
        &format!("answered_at = started_at, duration_secs = {duration}{extra}"),
    )
    .await;
}

#[test_db::test(seed)]
async fn stats_add_up_my_answered_calls_in_the_company_time_zone(pool: MySqlPool) {
    add_session(&pool, 87, 100, 100).await;
    let (server, _) = phone_server(pool.clone()).await;
    let stats = || async {
        server
            .get("/v1/phone/calls/stats")
            .add_header(HEADER, token(87))
            .await
            .json::<Value>()
    };
    for (round, zone) in ["America/Indiana/Indianapolis", "Asia/Tokyo"]
        .iter()
        .enumerate()
    {
        let tz = time_zone(zone).unwrap();
        let now = Utc::now();
        let (day, week, month) = (
            day_start(tz, now),
            week_start(tz, now),
            month_start(tz, now),
        );
        let base = 980_400 + 20 * i64::try_from(round).unwrap();
        sql(
            &pool,
            "DELETE FROM phone_calls WHERE id BETWEEN 980400 AND 980499",
        )
        .await;
        let second = Duration::seconds(1);
        talk(&pool, base, 100, day, Some(60), "").await;
        talk(&pool, base + 1, 100, day, None, "").await;
        talk(&pool, base + 2, 100, day - second, Some(30), "").await;
        talk(&pool, base + 3, 100, week - second, Some(20), "").await;
        talk(&pool, base + 4, 100, month - second, Some(10), "").await;
        talk(
            &pool,
            base + 5,
            100,
            month.min(week) - Duration::days(40),
            Some(7),
            "",
        )
        .await;
        // Not counted: Mark's call, a call nobody answered, a test call.
        talk(&pool, base + 6, 101, day, Some(500), "").await;
        add_call(
            &pool,
            base + 7,
            100,
            Some(100),
            "inbound",
            "missed",
            "+13175550100",
            &day.format("%Y-%m-%d %H:%M:%S").to_string(),
            "",
        )
        .await;
        talk(
            &pool,
            base + 8,
            100,
            day,
            Some(9),
            "other_number = '+13175550151', our_number = '+13175550151'",
        )
        .await;
        let in_week = |at: DateTime<Utc>| at >= week;
        let in_month = |at: DateTime<Utc>| at >= month;
        let mut week_secs = 60;
        let mut week_count = 2;
        let mut month_secs = 60;
        let mut month_count = 2;
        for (at, seconds) in [
            (day - second, 30),
            (week - second, 20),
            (month - second, 10),
        ] {
            if in_week(at) {
                week_secs += seconds;
                week_count += 1;
            }
            if in_month(at) {
                month_secs += seconds;
                month_count += 1;
            }
        }
        if *zone == "Asia/Tokyo" {
            enable_calling(&pool, 100, &format!(
                "business_hours = '{{\"tz\":\"{zone}\",\"days\":[{{\"day\":1,\"start\":\"09:00\",\"end\":\"17:00\"}}]}}'"
            ))
            .await;
        }
        assert_eq!(
            stats().await,
            json!({ "todaySecs": 60, "weekSecs": week_secs, "monthSecs": month_secs,
                    "todayCount": 2, "weekCount": week_count, "monthCount": month_count }),
            "{zone}"
        );
    }
}

#[test_db::test(seed)]
async fn recordings_are_for_the_rep_their_line_and_managers(pool: MySqlPool) {
    lines(&pool).await;
    calls(&pool).await;
    add_call(
        &pool,
        980_320,
        100,
        Some(100),
        "outbound",
        "answered",
        "+13175550100",
        "2026-10-12 12:00:00",
        "recording_status = 'pending', recording_key = 'phone/100/x.mp3'",
    )
    .await;
    for (n, user) in [(88, 100), (89, 101), (90, 102), (91, 107), (92, 106)] {
        add_session(&pool, n, user, if user == 106 { 101 } else { 100 }).await;
    }
    let (server, _) = phone_server(pool.clone()).await;
    let recording = |n: u32, id: i64| {
        server
            .get(&format!("/v1/phone/calls/{id}/recording"))
            .add_header(HEADER, token(n))
    };
    let rita = recording(88, 980_301).await;
    rita.assert_status_ok();
    let body: Value = rita.json();
    let expires = DateTime::parse_from_rfc3339(body["expiresAt"].as_str().unwrap()).unwrap();
    assert!(
        ((expires.with_timezone(&Utc) - Utc::now()) - Duration::seconds(300))
            .num_seconds()
            .abs()
            < 30
    );
    assert_eq!(
        body,
        json!({ "url": "https://files.test/bucket/phone/100/2026/10/980301-call.mp3",
                "expiresAt": body["expiresAt"], "contentType": "audio/mpeg" })
    );
    // My line's call, answered by Mark: mine (and his) to play.
    assert_eq!(
        recording(88, 980_302).await.json::<Value>()["contentType"],
        "audio/wav"
    );
    recording(89, 980_302).await.assert_status_ok();
    // A sales manager and an admin may play any; Dina may not.
    recording(89, 980_301).await.assert_status_ok();
    recording(90, 980_301).await.assert_status_ok();
    recording(91, 980_301)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    recording(91, 980_302)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    // No recording (yet), another company's call.
    for id in [980_303, 980_320, 999_999] {
        recording(88, id).await.assert_status_not_found();
    }
    recording(92, 980_301).await.assert_status_not_found();
    // Without a recordings bucket on the server.
    let (bare, _) = phone_server_with(pool.clone(), |state| {
        state.with_phone_config(api::phone::PhoneConfig::default())
    })
    .await;
    bare.get("/v1/phone/calls/980301/recording")
        .add_header(HEADER, token(88))
        .await
        .assert_status(StatusCode::SERVICE_UNAVAILABLE);
}

#[test_db::test(seed)]
async fn notes_go_on_the_calls_deal_or_the_customers_open_one(pool: MySqlPool) {
    lines(&pool).await;
    calls(&pool).await;
    // Erin's call has no deal; this one's deal was deleted.
    add_call(
        &pool,
        980_312,
        100,
        Some(100),
        "inbound",
        "answered",
        "+13175550105",
        "2026-10-12 13:00:00",
        "customer_id = 1004",
    )
    .await;
    add_call(
        &pool,
        980_313,
        100,
        Some(100),
        "inbound",
        "answered",
        "+13175550100",
        "2026-10-12 14:00:00",
        "customer_id = 1000, deal_id = 8005",
    )
    .await;
    add_session(&pool, 93, 101, 100).await;
    let (server, _) = phone_server(pool.clone()).await;
    let note = |id: i64, text: &str| {
        server
            .post(&format!("/v1/phone/calls/{id}/note"))
            .add_header(HEADER, token(93))
            .json(&json!({ "text": text }))
    };
    let added = note(980_301, "  Wants a quote by Friday ").await;
    added.assert_status(StatusCode::CREATED);
    let note_id = added.json::<Value>()["noteId"].as_i64().unwrap();
    let row: (i64, i32, String, Option<String>) = sqlx::query_as(
        "SELECT CAST(deal_id AS SIGNED), company_id, content, created_by FROM deal_notes WHERE id = ?",
    )
    .bind(note_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            8007,
            100,
            "Wants a quote by Friday".into(),
            Some("Mark Manager".into())
        )
    );
    // Rita (the deal's rep) hears about it, not Mark himself.
    let told: Vec<(i32, String)> = sqlx::query_as(
        "SELECT user_id, notification_type FROM notifications WHERE deal_id = 8007 ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(told, [(100, "note_added".to_string())]);
    let db = &pool;
    let deal_of = |response: axum_test::TestResponse| async move {
        response.assert_status(StatusCode::CREATED);
        let id = response.json::<Value>()["noteId"].as_i64().unwrap();
        sqlx::query_scalar::<_, i64>("SELECT CAST(deal_id AS SIGNED) FROM deal_notes WHERE id = ?")
            .bind(id)
            .fetch_one(db)
            .await
            .unwrap()
    };
    assert_eq!(deal_of(note(980_312, "Erin").await).await, 8002);
    assert_eq!(deal_of(note(980_313, "Alice").await).await, 8007);
    // No deal to put it on: Bob's only deal is lost; an unknown caller.
    for id in [980_310, 980_303] {
        let response = note(id, "x").await;
        response.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(response.json::<Value>(), json!({ "error": "no_deal" }));
    }
    note(980_301, "   ")
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    note(980_301, &"x".repeat(5001))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    note(980_308, "x").await.assert_status_not_found();
}

#[test_db::test(seed)]
async fn call_via_my_cell_rings_my_cell_then_the_customer(pool: MySqlPool) {
    enable_calling(&pool, 100, "daily_call_limit = 3").await;
    lines(&pool).await;
    set_settings(
        &pool,
        100,
        100,
        "cell_number = '+13175550177', cell_verified_at = UTC_TIMESTAMP(), default_caller_number_id = 9741",
    )
    .await;
    add_phone(&pool, 94, 100, 100).await;
    let (server, stubs) = phone_server(pool.clone()).await;
    let from_phone = |body: Value| {
        server
            .post("/v1/phone/calls/via-cell")
            .add_header(HEADER, token(94))
            .json(&body)
    };
    let started =
        from_phone(json!({ "to": "(317) 555-0100", "callerNumberId": null, "customerId": null }))
            .await;
    started.assert_status(StatusCode::ACCEPTED);
    let first = started.json::<Value>()["callId"].as_i64().unwrap();
    let base = webhook_base(100);
    assert_eq!(
        stubs.telnyx(),
        [(
            "POST".to_string(),
            "/texml/calls/app-100".to_string(),
            json!({ "To": "+13175550177", "From": "+13175550151", "Method": "POST",
                    "Url": format!("{base}/via-cell?call={first}"),
                    "StatusCallback": format!("{base}/call-status?call={first}"),
                    "StatusCallbackMethod": "POST" })
        )]
    );
    let row: (
        i32,
        String,
        String,
        String,
        Option<i32>,
        Option<i64>,
        Option<i32>,
        Option<String>,
        bool,
        Option<String>,
        bool,
        bool,
    ) = sqlx::query_as(
        "SELECT user_id, CAST(direction AS CHAR), CAST(status AS CHAR), other_number, customer_id,
                CAST(deal_id AS SIGNED), number_id, caller_id, via_cell <> 0, telnyx_call_sid,
                ai_notes <> 0, approved <> 0
           FROM phone_calls WHERE id = ? AND company_id = 100",
    )
    .bind(first)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            100,
            "outbound".into(),
            "ringing".into(),
            "+13175550100".into(),
            Some(1000),
            Some(8007),
            Some(9741),
            Some("+13175550151".into()),
            true,
            Some("v3:sid-1".into()),
            true,
            true
        )
    );
    let call: Value = server
        .get(&format!("/v1/phone/calls/{first}"))
        .add_header(HEADER, token(94))
        .await
        .json();
    assert_eq!(
        (&call["viaCell"], &call["status"], &call["customer"]["name"]),
        (&json!(true), &json!("ringing"), &json!("Alice Lead"))
    );
    // From the CRM (cookie), with a caller ID and a customer picked; a
    // customer of another company is ignored.
    let crm = server
        .post("/v1/phone/calls/via-cell")
        .with_session(sessions::REP)
        .json(&json!({ "to": "3175550102", "callerNumberId": 9742, "customerId": 1004 }))
        .await;
    crm.assert_status(StatusCode::ACCEPTED);
    let second = crm.json::<Value>()["callId"].as_i64().unwrap();
    // Telnyx fails: the row is failed.
    stubs.telnyx_down.store(true, Ordering::SeqCst);
    let failed = from_phone(json!({ "to": "3175550103", "customerId": 2000 })).await;
    failed.assert_status(StatusCode::BAD_GATEWAY);
    stubs.telnyx_down.store(false, Ordering::SeqCst);
    let rows: Vec<(i64, Option<i32>, Option<i32>, String)> = sqlx::query_as(
        "SELECT id, customer_id, number_id, CAST(status AS CHAR) FROM phone_calls
          WHERE id > ? ORDER BY id",
    )
    .bind(first)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0],
        (second, Some(1004), Some(9742), "ringing".to_string())
    );
    assert_eq!((rows[1].1, rows[1].3.as_str()), (Some(1002), "failed"));
    // The company's daily limit (3 a day per user).
    let limited = from_phone(json!({ "to": "3175550104" })).await;
    limited.assert_status(StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(limited.json::<Value>()["error"], "rate_limited");
    // Six a minute (the call Telnyx did not place does not count).
    sql(
        &pool,
        "UPDATE telnyx_company_config SET daily_call_limit = 100 WHERE company_id = 100",
    )
    .await;
    for _ in 0..4 {
        from_phone(json!({ "to": "3175550104" }))
            .await
            .assert_status(StatusCode::ACCEPTED);
    }
    let minute = from_phone(json!({ "to": "3175550104" })).await;
    minute.assert_status(StatusCode::TOO_MANY_REQUESTS);
    let wait = minute.json::<Value>()["retryAfterSecs"].as_u64().unwrap();
    assert!((1..=60).contains(&wait), "{wait}");
}

#[test_db::test(seed)]
async fn six_a_minute_hold_for_calls_started_at_once(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    lines(&pool).await;
    set_settings(
        &pool,
        100,
        100,
        "cell_number = '+13175550177', cell_verified_at = UTC_TIMESTAMP()",
    )
    .await;
    // Rita's phone and the CRM (another container, as far as she knows).
    add_phone(&pool, 93, 100, 100).await;
    let (server, stubs) = phone_server(pool.clone()).await;
    let call = |from_crm: bool| {
        let request = server.post("/v1/phone/calls/via-cell");
        let request = if from_crm {
            request.with_session(sessions::REP)
        } else {
            request.add_header(HEADER, token(93))
        };
        request.json(&json!({ "to": "3175550104" }))
    };
    let answers = futures_util::future::join_all(
        (0..8).map(|n| std::future::IntoFuture::into_future(call(n % 2 == 0))),
    )
    .await;
    let mut statuses: Vec<u16> = answers
        .iter()
        .map(|answer| answer.status_code().as_u16())
        .collect();
    statuses.sort_unstable();
    assert_eq!(statuses, [202, 202, 202, 202, 202, 202, 429, 429]);
    assert_eq!(stubs.telnyx().len(), 6);
}

#[test_db::test(seed)]
async fn call_via_my_cell_checks_everything_first(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    lines(&pool).await;
    set_settings(
        &pool,
        100,
        100,
        "cell_number = '+13175550177', cell_verified_at = UTC_TIMESTAMP()",
    )
    .await;
    add_phone(&pool, 95, 100, 100).await;
    add_phone(&pool, 96, 100, 100).await;
    revoke_session(&pool, 96).await;
    let (server, stubs) = phone_server(pool.clone()).await;
    let via = |body: Value| {
        server
            .post("/v1/phone/calls/via-cell")
            .add_header(HEADER, token(95))
            .json(&body)
    };
    for (body, error) in [
        (json!({ "to": "+442079460958" }), "invalid_phone"),
        (json!({ "to": "911" }), "invalid_phone"),
        (
            json!({ "to": "3175550100", "callerNumberId": 9744 }),
            "not_a_caller_id",
        ),
        (
            json!({ "to": "3175550100", "callerNumberId": 9743 }),
            "not_a_caller_id",
        ),
    ] {
        let response = via(body).await;
        response.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(response.json::<Value>(), json!({ "error": error }));
    }
    // Mark has no verified cell.
    let no_cell = server
        .post("/v1/phone/calls/via-cell")
        .with_session(sessions::MANAGER)
        .json(&json!({ "to": "3175550100" }))
        .await;
    no_cell.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(no_cell.json::<Value>(), json!({ "error": "no_cell" }));
    // Company 101 has calling off.
    let off = server
        .post("/v1/phone/calls/via-cell")
        .with_session(sessions::OTHER_COMPANY_REP)
        .json(&json!({ "to": "3175550100" }))
        .await;
    off.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(off.json::<Value>(), json!({ "error": "calling_disabled" }));
    // Not an employee; nobody; a revoked phone even with a good cookie.
    server
        .post("/v1/phone/calls/via-cell")
        .with_session(sessions::VIEWER)
        .json(&json!({ "to": "3175550100" }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .post("/v1/phone/calls/via-cell")
        .json(&json!({ "to": "3175550100" }))
        .await
        .assert_status_unauthorized();
    server
        .post("/v1/phone/calls/via-cell")
        .add_header(HEADER, token(96))
        .with_session(sessions::REP)
        .json(&json!({ "to": "3175550100" }))
        .await
        .assert_status_unauthorized();
    assert!(stubs.telnyx().is_empty());
    let placed: i64 = sqlx::query_scalar("SELECT CAST(COUNT(*) AS SIGNED) FROM phone_calls")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(placed, 0);
}

#[test_db::test(seed)]
async fn test_calls_ring_this_phone_and_stay_out_of_recents(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    add_number(&pool, 9743, 100, "+13175550150", "main", "Main line").await;
    add_phone(&pool, 97, 100, 100).await;
    let (server, stubs) = phone_server(pool.clone()).await;
    // The app sends no body.
    let response = server
        .post("/v1/phone/calls/test")
        .add_header(HEADER, token(97))
        .await;
    response.assert_status(StatusCode::ACCEPTED);
    let id = response.json::<Value>()["callId"].as_i64().unwrap();
    let base = webhook_base(100);
    assert_eq!(
        stubs.telnyx(),
        [(
            "POST".to_string(),
            "/texml/calls/app-100".to_string(),
            json!({ "To": "sip:gencred97@sip.telnyx.com", "From": "+13175550150", "Method": "POST",
                    "Url": format!("{base}/test-call?call={id}"),
                    "StatusCallback": format!("{base}/call-status?call={id}"),
                    "StatusCallbackMethod": "POST" })
        )]
    );
    assert_eq!(ids(&recents(&server, 97, "").await), Vec::<i64>::new());
    let call: Value = server
        .get(&format!("/v1/phone/calls/{id}"))
        .add_header(HEADER, token(97))
        .await
        .json();
    assert_eq!(
        (
            &call["direction"],
            &call["status"],
            &call["otherNumber"],
            &call["userId"]
        ),
        (
            &json!("inbound"),
            &json!("ringing"),
            &json!("+13175550150"),
            &json!(100)
        )
    );
    let transcribed: bool =
        sqlx::query_scalar("SELECT ai_notes <> 0 FROM phone_calls WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!transcribed);
    // A phone without a credential gets one first; `{}` works too.
    add_phone(&pool, 98, 100, 100).await;
    sql(&pool, "UPDATE phone_devices SET telnyx_credential_id = NULL, sip_username = NULL WHERE id = 960098").await;
    stubs.clear_telnyx();
    server
        .post("/v1/phone/calls/test")
        .add_header(HEADER, token(98))
        .json(&json!({}))
        .await
        .assert_status(StatusCode::ACCEPTED);
    assert_eq!(
        stubs.telnyx_paths(),
        ["POST /telephony_credentials", "POST /texml/calls/app-100"]
    );
    assert_eq!(stubs.telnyx()[1].2["To"], "sip:gencrednew1@sip.telnyx.com");
    // An unregistered phone; a company with calling off.
    add_session(&pool, 99, 100, 100).await;
    server
        .post("/v1/phone/calls/test")
        .add_header(HEADER, token(99))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    add_phone(&pool, 100, 106, 101).await;
    server
        .post("/v1/phone/calls/test")
        .add_header(HEADER, token(100))
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

/// A device leg of a call.
async fn leg(pool: &MySqlPool, call_id: i64, device_id: i64, sid: &str, status: &str) {
    sql(
        pool,
        &format!(
            "INSERT INTO phone_call_legs (call_id, device_id, leg_call_sid, status)
             VALUES ({call_id}, {device_id}, '{sid}', '{status}')"
        ),
    )
    .await;
}

async fn leg_status(pool: &MySqlPool, sid: &str) -> String {
    sqlx::query_scalar("SELECT status FROM phone_call_legs WHERE leg_call_sid = ?")
        .bind(sid)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[test_db::test(seed)]
async fn declining_moves_the_call_on_when_only_my_phones_ring(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    lines(&pool).await;
    let rita = add_phone(&pool, 101, 100, 100).await;
    let rita_2 = add_phone(&pool, 102, 100, 100).await;
    let mark = add_phone(&pool, 103, 101, 100).await;
    add_call(
        &pool,
        980_501,
        100,
        None,
        "inbound",
        "ringing",
        "+13175550199",
        "2026-10-18 10:00:00",
        "number_id = 9741, telnyx_call_sid = 'v3:parent-1'",
    )
    .await;
    leg(&pool, 980_501, rita, "leg-1a", "ringing").await;
    leg(&pool, 980_501, rita_2, "leg-1b", "ringing").await;
    add_call(
        &pool,
        980_502,
        100,
        None,
        "inbound",
        "ringing",
        "+13175550199",
        "2026-10-18 10:01:00",
        "number_id = 9742, telnyx_call_sid = 'v3:parent-2'",
    )
    .await;
    leg(&pool, 980_502, rita, "leg-2a", "ringing").await;
    leg(&pool, 980_502, mark, "leg-2b", "ringing").await;
    add_call(
        &pool,
        980_503,
        100,
        Some(100),
        "inbound",
        "in_progress",
        "+13175550199",
        "2026-10-18 10:02:00",
        "number_id = 9741, telnyx_call_sid = 'v3:parent-3', answered_at = '2026-10-18 10:02:05'",
    )
    .await;
    leg(&pool, 980_503, rita, "leg-3a", "ringing").await;
    leg(&pool, 980_503, rita_2, "leg-3b", "in-progress").await;
    let (server, stubs) = phone_server(pool.clone()).await;
    let decline = |id: &str| {
        server
            .post("/v1/phone/calls/decline")
            .add_header(HEADER, token(101))
            .json(&json!({ "telnyxCallId": id }))
    };
    // Only my phones ring: the caller goes on to the no-answer action.
    decline("leg-1a")
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(leg_status(&pool, "leg-1a").await, "declined");
    assert_eq!(
        stubs.telnyx(),
        [(
            "POST".to_string(),
            "/texml/calls/v3:parent-1/update".to_string(),
            json!({ "Url": format!("{}/dial-result?call=980501&declined=1", webhook_base(100)), "Method": "POST" })
        )]
    );
    stubs.clear_telnyx();
    // Mark's phone still rings on the sales line; an answered call; an
    // unknown id; Mark's leg (not this phone's).
    for id in ["leg-2a", "leg-3a", "nope", "leg-2b"] {
        decline(id).await.assert_status(StatusCode::NO_CONTENT);
    }
    assert!(stubs.telnyx().is_empty());
    assert_eq!(
        (
            leg_status(&pool, "leg-2a").await,
            leg_status(&pool, "leg-3a").await,
            leg_status(&pool, "leg-2b").await
        ),
        (
            "declined".to_string(),
            "declined".to_string(),
            "ringing".to_string()
        )
    );
    decline("  ").await.assert_status(StatusCode::BAD_REQUEST);
}

#[test_db::test(seed)]
async fn dialing_911_alerts_the_companys_admins(pool: MySqlPool) {
    // Ada (admin) has two phones, one whose app is gone; Sam (super admin
    // of 100, signed in to 101) gets the notification but no push there.
    add_phone(&pool, 104, 102, 100).await;
    add_phone(&pool, 105, 102, 100).await;
    sql(
        &pool,
        &format!("UPDATE phone_devices SET push_token = '{GONE_PUSH}' WHERE id = 960105"),
    )
    .await;
    add_phone(&pool, 106, 103, 101).await;
    add_phone(&pool, 107, 101, 100).await;
    add_phone(&pool, 108, 100, 100).await;
    let (server, stubs) = phone_server(pool.clone()).await;
    server
        .post("/v1/phone/e911")
        .add_header(HEADER, token(108))
        .json(&json!({ "number": "911" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let told: Vec<(i32, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT user_id, message, notification_type, actor_name FROM notifications
          WHERE notification_type = 'e911_alert' ORDER BY user_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let alert = |user: i32| {
        (
            user,
            "Rita Rep dialed 911 on Granite Phone".to_string(),
            Some("e911_alert".to_string()),
            Some("Rita Rep".to_string()),
        )
    };
    assert_eq!(told, [alert(102), alert(103)]);
    assert_eq!(
        stubs.pushes(),
        [
            json!({ "token": "push-104", "data": { "granite": "1", "type": "e911_alert" },
                 "android": { "priority": "HIGH", "ttl": "60s" },
                 "apns": { "headers": { "apns-push-type": "alert", "apns-priority": "10" },
                           "payload": { "aps": {
                               "alert": { "title": "911 was dialed",
                                          "body": "Someone in your company dialed 911. Open Granite Phone for details." },
                               "sound": "default", "mutable-content": 1 } } } })
        ]
    );
    let gone: Option<String> =
        sqlx::query_scalar("SELECT push_token FROM phone_devices WHERE id = 960105")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(gone, None);
    let alerts = || async {
        let messages: Vec<String> = sqlx::query_scalar(
            "SELECT message FROM notifications WHERE notification_type = 'e911_alert' ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        messages
    };
    let dial = |n: u32, number: &'static str| {
        server
            .post("/v1/phone/e911")
            .add_header(HEADER, token(n))
            .json(&json!({ "number": number }))
    };
    // The same phone within 10 minutes: no new alert, whatever it dials.
    for number in ["911", "411"] {
        dial(108, number)
            .await
            .assert_status(StatusCode::NO_CONTENT);
    }
    assert_eq!(alerts().await.len(), 2);
    // Mark's phone: 411 alerts, and so does an emergency right after it;
    // "1112" is read as 112.
    dial(107, "411").await.assert_status(StatusCode::NO_CONTENT);
    dial(107, "1112")
        .await
        .assert_status(StatusCode::NO_CONTENT);
    dial(107, "933").await.assert_status(StatusCode::NO_CONTENT);
    let messages = alerts().await;
    assert_eq!(
        messages[2..],
        [
            "Mark Manager dialed 411 on Granite Phone".to_string(),
            "Mark Manager dialed 411 on Granite Phone".to_string(),
            "Mark Manager dialed 112 on Granite Phone".to_string(),
            "Mark Manager dialed 112 on Granite Phone".to_string(),
        ]
    );
    for number in ["3175550100", "999", ""] {
        let response = server
            .post("/v1/phone/e911")
            .add_header(HEADER, token(108))
            .json(&json!({ "number": number }))
            .await;
        response.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(
            response.json::<Value>(),
            json!({ "error": "invalid_phone" })
        );
    }
}

#[test_db::test(seed)]
async fn call_routes_take_only_a_live_device_token(pool: MySqlPool) {
    lines(&pool).await;
    calls(&pool).await;
    add_phone(&pool, 109, 100, 100).await;
    revoke_session(&pool, 109).await;
    let (server, _) = phone_server(pool.clone()).await;
    assert_device_only(
        &server,
        109,
        &[
            (Method::GET, "/v1/phone/calls".to_string()),
            (Method::GET, "/v1/phone/calls/980301".to_string()),
            (Method::DELETE, "/v1/phone/calls/980301".to_string()),
            (Method::POST, "/v1/phone/calls/seen".to_string()),
            (Method::GET, "/v1/phone/calls/stats".to_string()),
            (Method::GET, "/v1/phone/calls/980301/recording".to_string()),
            (Method::POST, "/v1/phone/calls/980301/note".to_string()),
            (Method::POST, "/v1/phone/calls/test".to_string()),
            (Method::POST, "/v1/phone/calls/decline".to_string()),
            (Method::POST, "/v1/phone/e911".to_string()),
        ],
    )
    .await;
}

#[test_db::test(seed)]
async fn recordings_and_ai_notes_are_for_those_who_may_hear_the_call(pool: MySqlPool) {
    lines(&pool).await;
    // Recorded calls, each with an AI note: a voicemail on the sales line
    // (rings Rita and Mark), one on the main line (assigned to Rita, not
    // ringing her), Mark's answered call on the sales line, and Mark's call
    // on Rita's deal 8000.
    let recorded = |id: i64| {
        format!(
            "recording_status = 'copied', recording_key = 'phone/100/2026/10/{id}-call.mp3',
             transcript_status = 'done', ai_note = 'Note {id}'"
        )
    };
    add_call(
        &pool,
        980_340,
        100,
        None,
        "inbound",
        "voicemail",
        "+13175550197",
        "2026-10-12 09:00:00",
        &format!(
            "number_id = 9742, ended_at = '2026-10-12 09:01:00', {}",
            recorded(980_340)
        ),
    )
    .await;
    add_call(
        &pool,
        980_341,
        100,
        None,
        "inbound",
        "voicemail",
        "+13175550197",
        "2026-10-12 10:00:00",
        &format!(
            "number_id = 9743, ended_at = '2026-10-12 10:01:00', {}",
            recorded(980_341)
        ),
    )
    .await;
    add_call(
        &pool,
        980_342,
        100,
        Some(101),
        "inbound",
        "answered",
        "+13175550198",
        "2026-10-12 11:00:00",
        &format!(
            "number_id = 9742, answered_at = '2026-10-12 11:00:03', {}",
            recorded(980_342)
        ),
    )
    .await;
    add_call(&pool, 980_343, 100, Some(101), "outbound", "answered", "+13175550100", "2026-10-12 12:00:00",
        &format!("customer_id = 1000, deal_id = 8000, number_id = 9744, answered_at = '2026-10-12 12:00:03', {}",
                 recorded(980_343))).await;
    // Rita, Mark (a sales manager), Ada (admin), Dina (none of these).
    for (n, user) in [(93, 100), (94, 101), (95, 102), (96, 107)] {
        add_session(&pool, n, user, 100).await;
    }
    let (server, _) = phone_server(pool.clone()).await;
    let hears = |n: u32, id: i64| {
        let server = &server;
        async move {
            let status = server
                .get(&format!("/v1/phone/calls/{id}/recording"))
                .add_header(HEADER, token(n))
                .await
                .status_code();
            let call: Value = server
                .get(&format!("/v1/phone/calls/{id}"))
                .add_header(HEADER, token(n))
                .await
                .json();
            // The call row says the same as the recording route.
            let shown = call["hasRecording"] == true && call["aiNote"] == format!("Note {id}");
            assert_eq!(
                shown,
                status == StatusCode::OK,
                "phone {n}, call {id}: {call}"
            );
            if !shown {
                assert_eq!(
                    (&call["hasRecording"], &call["aiNote"]),
                    (&json!(false), &Value::Null)
                );
            }
            status
        }
    };
    let ok = StatusCode::OK;
    let no = StatusCode::FORBIDDEN;
    // The sales line's voicemail: the people it rings (the push's), not Dina.
    assert_eq!(hears(93, 980_340).await, ok);
    assert_eq!(hears(94, 980_340).await, ok);
    assert_eq!(hears(96, 980_340).await, no);
    // The main line does not ring Rita.
    assert_eq!(hears(93, 980_341).await, no);
    // Answered by Mark: no longer the sales line's, so not Rita's.
    assert_eq!(hears(93, 980_342).await, no);
    assert_eq!(hears(94, 980_342).await, ok);
    // Rita owns the deal of Mark's call; Dina does not.
    assert_eq!(hears(93, 980_343).await, ok);
    assert_eq!(hears(96, 980_343).await, no);
    // Admins and sales managers hear every call.
    for id in [980_340, 980_341, 980_342, 980_343] {
        assert_eq!(hears(95, id).await, ok);
        assert_eq!(hears(94, id).await, ok);
    }
    // The contact card hides them too.
    let card: Value = server
        .get("/v1/phone/contacts/1000")
        .add_header(HEADER, token(96))
        .await
        .json();
    let mark_call = card["calls"]
        .as_array()
        .unwrap()
        .iter()
        .find(|call| call["id"] == 980_343)
        .unwrap();
    assert_eq!(
        (&mark_call["hasRecording"], &mark_call["aiNote"]),
        (&json!(false), &Value::Null)
    );
}
