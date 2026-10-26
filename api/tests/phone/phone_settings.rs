//! Granite Phone settings: caller IDs, settings, availability, notices,
//! verifying my cell, and "Report a problem" (`routes/phone/settings.rs`,
//! `cell.rs`, `support.rs`).
//!
//! Fixture phones use n = 21..=40; numbers 9710+.

use crate::common;
use crate::phone_common;

use api::phone::clock::time_zone;
use axum::http::{Method, StatusCode};
use chrono::{Datelike, Duration, SecondsFormat, Utc};
use common::{WithSession, sessions};
use phone_common::{
    HEADER, REFUSED_NUMBER, add_number, add_phone, add_session, assert_device_only, assign,
    enable_calling, phone_server, revoke_session, set_settings, sql, token, webhook_base,
};
use serde_json::{Value, json};
use sqlx::MySqlPool;

/// Rita's lines: her direct line and the sales line she may call from, the
/// main line that only rings her, an inactive line, Mark's direct line, and
/// a company 101 line wrongly assigned to her.
async fn lines(pool: &MySqlPool) {
    add_number(pool, 9711, 100, "+13175550111", "direct", "Rita direct").await;
    add_number(pool, 9712, 100, "+13175550112", "shared", "Sales").await;
    add_number(pool, 9713, 100, "+13175550113", "main", "Main line").await;
    add_number(pool, 9714, 100, "+13175550114", "shared", "Old line").await;
    add_number(pool, 9715, 100, "+13175550115", "direct", "Mark direct").await;
    add_number(pool, 9716, 101, "+14635550116", "direct", "Other Co").await;
    sql(pool, "UPDATE telnyx_numbers SET active = 0 WHERE id = 9714").await;
    assign(pool, 9711, 100, true, true).await;
    assign(pool, 9712, 100, true, true).await;
    assign(pool, 9713, 100, true, false).await;
    assign(pool, 9714, 100, true, true).await;
    assign(pool, 9715, 101, true, true).await;
    assign(pool, 9716, 100, true, true).await;
}

fn defaults() -> Value {
    json!({
        "defaultCallerNumberId": null, "aiNotes": true, "workingHours": null,
        "cellNumber": null, "cellVerified": false, "cellFallback": false, "ringTimeoutSecs": 25,
        "forwardAll": false, "forwardAllTo": "voicemail", "forwardAllNumber": null,
                          "displayNumber": "caller", "unknownShowMine": false, "announceDirect": false,
                          "announceShared": false, "announceLineName": true, "requirePin": false,
                          "holdMusic": "calm", "holdMusicUrl": null, "holdMusicName": null,
    })
}

#[test_db::test(seed)]
async fn caller_ids_are_the_lines_i_may_call_from(pool: MySqlPool) {
    lines(&pool).await;
    add_session(&pool, 21, 100, 100).await;
    add_session(&pool, 22, 101, 100).await;
    let (server, _) = phone_server(pool.clone()).await;
    let caller_ids = |n: u32| {
        server
            .get("/v1/phone/caller-ids")
            .add_header(HEADER, token(n))
    };
    let rita = json!([
        { "numberId": 9711, "e164": "+13175550111", "label": "Rita direct", "kind": "direct" },
        { "numberId": 9712, "e164": "+13175550112", "label": "Sales", "kind": "shared" },
    ]);
    // Without a chosen default: her direct line.
    assert_eq!(
        caller_ids(21).await.json::<Value>(),
        json!({ "callerIds": rita, "defaultNumberId": 9711 })
    );
    set_settings(&pool, 100, 100, "default_caller_number_id = 9712").await;
    assert_eq!(
        caller_ids(21).await.json::<Value>()["defaultNumberId"],
        9712
    );
    // A default that is no longer hers falls back.
    sql(
        &pool,
        "UPDATE phone_user_settings SET default_caller_number_id = 9715 WHERE user_id = 100",
    )
    .await;
    assert_eq!(
        caller_ids(21).await.json::<Value>()["defaultNumberId"],
        9711
    );
    assert_eq!(
        caller_ids(22).await.json::<Value>(),
        json!({ "callerIds": [
            { "numberId": 9715, "e164": "+13175550115", "label": "Mark direct", "kind": "direct" },
        ], "defaultNumberId": 9715 })
    );
    // Olga sees none of company 100's lines.
    add_session(&pool, 23, 106, 101).await;
    assert_eq!(
        caller_ids(23).await.json::<Value>(),
        json!({ "callerIds": [], "defaultNumberId": null })
    );
}

#[test_db::test(seed)]
async fn settings_change_only_what_is_sent(pool: MySqlPool) {
    lines(&pool).await;
    add_session(&pool, 24, 100, 100).await;
    // A row left from when Rita was in another company does not count.
    set_settings(&pool, 100, 101, "ai_notes = 0, ring_timeout_secs = 50").await;
    let (server, _) = phone_server(pool.clone()).await;
    let put = |body: Value| {
        server
            .put("/v1/phone/settings")
            .add_header(HEADER, token(24))
            .json(&body)
    };
    let unchanged = put(json!({})).await;
    unchanged.assert_status_ok();
    assert_eq!(unchanged.json::<Value>(), defaults());
    let hours = json!({ "tz": " America/Chicago ", "days": [
        { "day": 1, "start": "08:00", "end": "12:00" },
        { "day": 1, "start": "13:00", "end": "17:30" },
        { "day": 6, "start": "09:00", "end": "24:00" },
    ] });
    let changed = put(json!({
        "defaultCallerNumberId": 9712, "aiNotes": false, "ringTimeoutSecs": 40, "workingHours": hours,
        "cellNumber": "+13175550100",
    }))
    .await;
    changed.assert_status_ok();
    let expected = json!({
        "defaultCallerNumberId": 9712, "aiNotes": false,
        "workingHours": { "tz": "America/Chicago", "days": [
            { "day": 1, "start": "08:00", "end": "12:00" },
            { "day": 1, "start": "13:00", "end": "17:30" },
            { "day": 6, "start": "09:00", "end": "24:00" },
        ] },
        "cellNumber": null, "cellVerified": false, "cellFallback": false, "ringTimeoutSecs": 40,
        "forwardAll": false, "forwardAllTo": "voicemail", "forwardAllNumber": null,
                          "displayNumber": "caller", "unknownShowMine": false, "announceDirect": false,
                          "announceShared": false, "announceLineName": true, "requirePin": false,
                          "holdMusic": "calm", "holdMusicUrl": null, "holdMusicName": null,
    });
    assert_eq!(changed.json::<Value>(), expected);
    // What is left out stays; null clears.
    let cleared = put(json!({ "defaultCallerNumberId": null, "workingHours": null })).await;
    let mut now = expected.clone();
    now["defaultCallerNumberId"] = Value::Null;
    now["workingHours"] = Value::Null;
    assert_eq!(cleared.json::<Value>(), now);
    let stored: (i32, i64) = sqlx::query_as(
        "SELECT company_id, CAST(ring_timeout_secs AS SIGNED) FROM phone_user_settings WHERE user_id = 100",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored, (100, 40));

    // Refused, and nothing changes.
    for (body, error) in [
        (json!({ "defaultCallerNumberId": 9715 }), "not_a_caller_id"),
        (json!({ "defaultCallerNumberId": 9713 }), "not_a_caller_id"),
        (json!({ "defaultCallerNumberId": 9716 }), "not_a_caller_id"),
        (json!({ "cellFallback": true }), "cell_not_verified"),
        (
            json!({ "ringTimeoutSecs": 14 }),
            "ringTimeoutSecs must be 15 to 60",
        ),
        (
            json!({ "ringTimeoutSecs": 61 }),
            "ringTimeoutSecs must be 15 to 60",
        ),
        (
            json!({ "workingHours": { "tz": "Mars/Olympus", "days": [{ "day": 1, "start": "08:00", "end": "17:00" }] } }),
            "Unknown time zone: Mars/Olympus",
        ),
        (
            json!({ "workingHours": { "tz": "America/Chicago", "days": [] } }),
            "Add at least one day, or clear the working hours.",
        ),
        (
            json!({ "workingHours": { "tz": "America/Chicago", "days": [{ "day": 2, "start": "17:00", "end": "08:00" }] } }),
            "Each range needs a start before its end, as HH:MM (got 17:00 to 08:00).",
        ),
    ] {
        let response = put(body.clone()).await;
        response.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(
            response.json::<Value>(),
            json!({ "error": error }),
            "{body}"
        );
    }
    put(json!({ "aiNotes": "yes" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(put(json!({})).await.json::<Value>(), now);
    // With a verified cell the fallback can be turned on.
    sql(
        &pool,
        "UPDATE phone_user_settings SET cell_number = '+13175550177', cell_verified_at = UTC_TIMESTAMP()
          WHERE user_id = 100",
    )
    .await;
    let fallback = put(json!({ "cellFallback": true })).await;
    fallback.assert_status_ok();
    let body: Value = fallback.json();
    assert_eq!(
        (
            &body["cellNumber"],
            &body["cellVerified"],
            &body["cellFallback"]
        ),
        (&json!("+13175550177"), &json!(true), &json!(true))
    );
}

#[test_db::test(seed)]
async fn availability_is_do_not_disturb_or_working_hours(pool: MySqlPool) {
    add_session(&pool, 25, 100, 100).await;
    let (server, _) = phone_server(pool.clone()).await;
    let put = |body: Value| {
        server
            .put("/v1/phone/availability")
            .add_header(HEADER, token(25))
            .json(&body)
    };
    let in_an_hour = (Utc::now() + Duration::hours(1)).to_rfc3339_opts(SecondsFormat::Secs, true);
    let timed = put(json!({ "status": "dnd", "dndUntil": in_an_hour })).await;
    timed.assert_status_ok();
    let until = in_an_hour.replace('Z', ".000Z");
    assert_eq!(
        timed.json::<Value>(),
        json!({ "status": "dnd", "dndUntil": until, "dndIndefinite": false })
    );
    let bootstrap: Value = server
        .get("/v1/phone/bootstrap")
        .add_header(HEADER, token(25))
        .await
        .json();
    assert_eq!(
        bootstrap["availability"],
        json!({ "status": "dnd", "dndUntil": until, "dndIndefinite": false })
    );
    assert_eq!(
        put(json!({ "status": "dnd", "dndUntil": null }))
            .await
            .json::<Value>(),
        json!({ "status": "dnd", "dndUntil": null, "dndIndefinite": true })
    );
    assert_eq!(
        put(json!({ "status": "dnd" })).await.json::<Value>(),
        json!({ "status": "dnd", "dndUntil": null, "dndIndefinite": true })
    );
    assert_eq!(
        put(json!({ "status": "available", "dndUntil": in_an_hour }))
            .await
            .json::<Value>(),
        json!({ "status": "available", "dndUntil": null, "dndIndefinite": false })
    );
    // Available, but working hours only tomorrow (local): outside hours.
    let tz = time_zone("America/Indiana/Indianapolis").unwrap();
    let tomorrow = (Utc::now()
        .with_timezone(&tz)
        .weekday()
        .num_days_from_sunday()
        + 1)
        % 7;
    server
        .put("/v1/phone/settings")
        .add_header(HEADER, token(25))
        .json(
            &json!({ "workingHours": { "tz": "America/Indiana/Indianapolis", "days": [
            { "day": tomorrow, "start": "00:00", "end": "24:00" } ] } }),
        )
        .await
        .assert_status_ok();
    assert_eq!(
        put(json!({ "status": "available" })).await.json::<Value>(),
        json!({ "status": "outside_hours", "dndUntil": null, "dndIndefinite": false })
    );
    // Do not disturb still wins.
    assert_eq!(
        put(json!({ "status": "dnd" })).await.json::<Value>()["status"],
        "dnd"
    );
    let past = (Utc::now() - Duration::minutes(1)).to_rfc3339();
    let far = (Utc::now() + Duration::days(400)).to_rfc3339();
    for body in [
        json!({ "status": "dnd", "dndUntil": past }),
        json!({ "status": "dnd", "dndUntil": far }),
        json!({ "status": "outside_hours" }),
        json!({ "status": "dnd", "dndUntil": "tomorrow" }),
    ] {
        put(body).await.assert_status(StatusCode::BAD_REQUEST);
    }
    let stored: (bool, bool) = sqlx::query_as(
        "SELECT dnd_until IS NULL, dnd_indefinite <> 0 FROM phone_user_settings WHERE user_id = 100",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored, (true, true));
}

#[test_db::test(seed)]
async fn notices_are_accepted_per_version(pool: MySqlPool) {
    let device = add_phone(&pool, 26, 100, 100).await;
    let (server, _) = phone_server(pool.clone()).await;
    let accept = |body: Value| {
        server
            .post("/v1/phone/notices/accept")
            .add_header(HEADER, token(26))
            .json(&body)
    };
    let notices = || async {
        server
            .get("/v1/phone/bootstrap")
            .add_header(HEADER, token(26))
            .await
            .json::<Value>()["notices"]
            .clone()
    };
    assert_eq!(
        notices().await,
        json!({ "recordingVersion": 1, "e911Version": 1, "acceptedRecordingVersion": null, "acceptedE911Version": null })
    );
    for _ in 0..2 {
        accept(json!({ "notice": "recording", "version": 1 }))
            .await
            .assert_status(StatusCode::NO_CONTENT);
    }
    assert_eq!(
        notices().await,
        json!({ "recordingVersion": 1, "e911Version": 1, "acceptedRecordingVersion": 1, "acceptedE911Version": null })
    );
    accept(json!({ "notice": "e911", "version": 1 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(notices().await["acceptedE911Version"], 1);
    let rows: Vec<(String, i32, Option<i64>)> = sqlx::query_as(
        "SELECT CAST(notice AS CHAR), version, device_id FROM phone_notice_acks WHERE user_id = 100 ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            ("recording".to_string(), 1, Some(device)),
            ("e911".to_string(), 1, Some(device))
        ]
    );
    for body in [
        json!({ "notice": "e911", "version": 2 }),
        json!({ "notice": "recording", "version": 0 }),
        json!({ "notice": "privacy", "version": 1 }),
    ] {
        accept(body).await.assert_status(StatusCode::BAD_REQUEST);
    }
}

/// Rita's pending cell code.
async fn pending_code(pool: &MySqlPool) -> (Option<String>, Option<String>, i64, i64) {
    sqlx::query_as(
        "SELECT cell_pending_number, cell_code,
                CAST(COALESCE(TIMESTAMPDIFF(SECOND, UTC_TIMESTAMP(), cell_code_expires_at), -1) AS SIGNED),
                CAST(cell_code_attempts AS SIGNED)
           FROM phone_user_settings WHERE user_id = 100",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

#[test_db::test(seed)]
async fn my_cell_is_verified_with_a_code_call(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    lines(&pool).await;
    add_session(&pool, 27, 100, 100).await;
    let (server, stubs) = phone_server(pool.clone()).await;
    let verify = |cell: &str| {
        server
            .post("/v1/phone/cell/verify")
            .add_header(HEADER, token(27))
            .json(&json!({ "cellNumber": cell }))
    };
    let confirm = |code: &str| {
        server
            .post("/v1/phone/cell/confirm")
            .add_header(HEADER, token(27))
            .json(&json!({ "code": code }))
    };
    let sent = verify("(317) 555-0177").await;
    sent.assert_status(StatusCode::ACCEPTED);
    assert_eq!(sent.text(), "");
    // Our server calls the cell from Rita's direct line; the call asks
    // `cell-code` what to say.
    assert_eq!(
        stubs.telnyx(),
        [(
            "POST".to_string(),
            "/texml/calls/app-100".to_string(),
            json!({ "To": "+13175550177", "From": "+13175550111", "Method": "POST",
                    "Url": format!("{}/cell-code?user=100", webhook_base(100)) })
        )]
    );
    let (pending, code, seconds_left, attempts) = pending_code(&pool).await;
    let code = code.unwrap();
    assert_eq!(pending.as_deref(), Some("+13175550177"));
    assert!(code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()));
    assert!((590..=600).contains(&seconds_left), "{seconds_left}");
    assert_eq!(attempts, 0);
    // One code call a minute.
    let again = verify("3175550177").await;
    again.assert_status(StatusCode::TOO_MANY_REQUESTS);
    let wait = again.json::<Value>()["retryAfterSecs"].as_u64().unwrap();
    assert!((1..=60).contains(&wait));
    assert_eq!(stubs.telnyx().len(), 1);

    let wrong = if code == "000000" { "111111" } else { "000000" };
    let refused = confirm(wrong).await;
    refused.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(refused.json::<Value>(), json!({ "error": "wrong_code" }));
    assert_eq!(pending_code(&pool).await.3, 1);
    let verified = confirm(&format!(" {code} ")).await;
    verified.assert_status_ok();
    let mut expected = defaults();
    expected["cellNumber"] = json!("+13175550177");
    expected["cellVerified"] = json!(true);
    assert_eq!(verified.json::<Value>(), expected);
    assert_eq!(pending_code(&pool).await, (None, None, -1, 0));
    // A used code is gone.
    confirm(&code).await.assert_status(StatusCode::BAD_REQUEST);
    // Confirming does not reopen the minute: still one code call a minute.
    verify("+1 317 555 0178")
        .await
        .assert_status(StatusCode::TOO_MANY_REQUESTS);
    let a_minute_later = "UPDATE phone_user_settings SET cell_code_sent_at = cell_code_sent_at - INTERVAL 61 SECOND WHERE user_id = 100";
    sql(&pool, a_minute_later).await;

    // A new number: five wrong tries void the code; the verified cell stays.
    verify("+1 317 555 0178")
        .await
        .assert_status(StatusCode::ACCEPTED);
    let code = pending_code(&pool).await.1.unwrap();
    let wrong = if code == "000000" { "111111" } else { "000000" };
    for _ in 0..5 {
        confirm(wrong).await.assert_status(StatusCode::BAD_REQUEST);
    }
    assert_eq!(pending_code(&pool).await, (None, None, -1, 0));
    confirm(&code).await.assert_status(StatusCode::BAD_REQUEST);
    let settings: Value = server
        .put("/v1/phone/settings")
        .add_header(HEADER, token(27))
        .json(&json!({}))
        .await
        .json();
    assert_eq!(settings["cellNumber"], "+13175550177");
    // An expired code is refused.
    sql(
        &pool,
        "UPDATE phone_user_settings SET cell_code = '123456', cell_pending_number = '+13175550179',
                cell_code_expires_at = UTC_TIMESTAMP() - INTERVAL 1 SECOND WHERE user_id = 100",
    )
    .await;
    confirm("123456")
        .await
        .assert_status(StatusCode::BAD_REQUEST);

    // Numbers outside the US and Canada, and bad ones.
    for bad in ["+442079460958", "555-0177", "+18765550142", "call me"] {
        let response = verify(bad).await;
        response.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(
            response.json::<Value>(),
            json!({ "error": "invalid_phone" })
        );
    }
    // Voiding the code by wrong tries does not reopen the minute either.
    verify("3175550180")
        .await
        .assert_status(StatusCode::TOO_MANY_REQUESTS);
    sql(&pool, a_minute_later).await;
    // When Telnyx refuses the call, the code is dropped and the call not
    // counted: try again at once.
    stubs.clear_telnyx();
    verify(REFUSED_NUMBER)
        .await
        .assert_status(StatusCode::BAD_GATEWAY);
    assert_eq!(pending_code(&pool).await.1, None);
    verify("3175550180")
        .await
        .assert_status(StatusCode::ACCEPTED);
    // Three code calls today: two more, then the day is used up.
    for _ in 0..2 {
        sql(&pool, a_minute_later).await;
        verify("3175550181")
            .await
            .assert_status(StatusCode::ACCEPTED);
    }
    sql(&pool, a_minute_later).await;
    let day = verify("3175550181").await;
    day.assert_status(StatusCode::TOO_MANY_REQUESTS);
    let wait = day.json::<Value>()["retryAfterSecs"].as_u64().unwrap();
    assert!((82_000..=86_400).contains(&wait), "{wait}");
    // A new day's window, but six calls this minute: a code call is a call.
    sql(&pool, "UPDATE phone_user_settings SET cell_code_window_start = UTC_TIMESTAMP() - INTERVAL 25 HOUR WHERE user_id = 100").await;
    for n in 0..6 {
        sql(
            &pool,
            &format!(
                "INSERT INTO phone_calls (company_id, user_id, direction, status, other_number, other_last10, started_at)
                 VALUES (100, 100, 'outbound', 'answered', '+1317555020{n}', '317555020{n}', UTC_TIMESTAMP(3))"
            ),
        )
        .await;
    }
    verify("3175550181")
        .await
        .assert_status(StatusCode::TOO_MANY_REQUESTS);
}

#[test_db::test(seed)]
async fn cell_codes_need_calling(pool: MySqlPool) {
    add_session(&pool, 28, 106, 101).await;
    let (server, stubs) = phone_server(pool.clone()).await;
    let response = server
        .post("/v1/phone/cell/verify")
        .add_header(HEADER, token(28))
        .json(&json!({ "cellNumber": "3175550177" }))
        .await;
    response.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(
        response.json::<Value>(),
        json!({ "error": "calling_disabled" })
    );
    // Calling on, but no number to call from.
    enable_calling(&pool, 101, "").await;
    server
        .post("/v1/phone/cell/verify")
        .add_header(HEADER, token(28))
        .json(&json!({ "cellNumber": "3175550177" }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    assert!(stubs.telnyx().is_empty());
}

#[test_db::test(seed)]
async fn problem_reports_reach_support_masked(pool: MySqlPool) {
    add_session(&pool, 29, 100, 100).await;
    let (server, stubs) = phone_server(pool.clone()).await;
    let jwt = format!(
        "eyJ{}.{}.{}",
        "a".repeat(40),
        "b".repeat(20),
        "c".repeat(10)
    );
    server
        .post("/v1/phone/diagnostics")
        .add_header(HEADER, token(29))
        .json(&json!({
            "appVersion": "1.4.0",
            "device": "samsung SM-S921U, Android 16",
            "summary": "Calls to +13175550142 <drop> after 5 s",
            "log": ["10:00:01 call to (317) 555-0199 failed", format!("login with {jwt}")],
        }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let emails = stubs.emails();
    assert_eq!(emails.len(), 1);
    let email = &emails[0];
    assert_eq!(
        (
            &email["to"],
            &email["subject"],
            &email["reply_to"],
            &email["from_address"],
            &email["transactional"]
        ),
        (
            &json!(["support@granite-manager.com"]),
            &json!("[Granite Phone] Problem report from Acme Stone"),
            &json!(["rep@acme.test"]),
            &json!("noreply@granite-manager.com"),
            &json!(true)
        )
    );
    let body = email["body"].as_str().unwrap();
    for shown in [
        "Rita Rep &lt;rep@acme.test&gt; (user 100), Acme Stone (company 100)",
        "1.4.0 on samsung SM-S921U, Android 16",
        "Calls to …0142 &lt;drop&gt; after 5 s",
        "10:00:01 call to …0199 failed\nlogin with [token]",
        "(2 lines, oldest first)",
    ] {
        assert!(body.contains(shown), "{shown} in {body}");
    }
    for hidden in ["3175550142", "555-0199", &jwt] {
        assert!(!body.contains(hidden), "{hidden} in {body}");
    }
    // Without the webhooks Lambda answering, the app hears it failed.
    let offline = axum_test::TestServer::new(api::app(api::state::AppState::new(
        pool.clone(),
        common::SECRET,
        common::ALLOWED_ORIGIN,
    )));
    offline
        .post("/v1/phone/diagnostics")
        .add_header(HEADER, token(29))
        .json(&json!({ "appVersion": "1", "device": "x", "summary": "y", "log": [] }))
        .await
        .assert_status(StatusCode::BAD_GATEWAY);
    // Three reports an hour per phone (the one that failed did not count).
    let report = || {
        server
            .post("/v1/phone/diagnostics")
            .add_header(HEADER, token(29))
            .json(&json!({ "appVersion": "1", "device": "x", "summary": "again", "log": [] }))
    };
    for _ in 0..2 {
        report().await.assert_status(StatusCode::NO_CONTENT);
    }
    let limited = report().await;
    limited.assert_status(StatusCode::TOO_MANY_REQUESTS);
    let wait = limited.json::<Value>()["retryAfterSecs"].as_u64().unwrap();
    assert!((3_500..=3_600).contains(&wait), "{wait}");
    assert_eq!(stubs.emails().len(), 3);
    // An hour on, the phone may report again.
    sql(&pool, "UPDATE device_sessions SET diagnostics_window_start = UTC_TIMESTAMP() - INTERVAL 61 MINUTE WHERE id = 950029").await;
    report().await.assert_status(StatusCode::NO_CONTENT);
}

#[test_db::test(seed)]
async fn settings_routes_take_only_a_live_device_token(pool: MySqlPool) {
    add_phone(&pool, 30, 100, 100).await;
    revoke_session(&pool, 30).await;
    let (server, _) = phone_server(pool.clone()).await;
    assert_device_only(
        &server,
        30,
        &[
            (Method::GET, "/v1/phone/caller-ids".to_string()),
            (Method::PUT, "/v1/phone/settings".to_string()),
            (Method::PUT, "/v1/phone/availability".to_string()),
            (Method::POST, "/v1/phone/notices/accept".to_string()),
            (Method::POST, "/v1/phone/diagnostics".to_string()),
        ],
    )
    .await;
    // Cell verification takes the CRM cookie too (`tests/phone_web.rs`); a
    // device token, when sent, still decides alone, and nothing is 401.
    for path in ["/v1/phone/cell/verify", "/v1/phone/cell/confirm"] {
        for request in [
            server.post(path),
            server.post(path).add_header(HEADER, token(30)),
            server
                .post(path)
                .add_header(HEADER, token(30))
                .with_session(sessions::REP),
        ] {
            request
                .json(&json!({}))
                .await
                .assert_status(StatusCode::UNAUTHORIZED);
        }
    }
}
