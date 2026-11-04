//! Granite Phone's job (`routes/jobs/phone.rs`, `POST /v1/jobs/phone`):
//! signing out users who left, leftover Telnyx credentials, old callbacks,
//! recording copy retries, the customer phone index, AI notes (AssemblyAI,
//! then OpenAI, the CRM's clean-up, then the deal) and call points.
//!
//! Fixture phones use n = 131..=150; calls 981000+; customers 931000+.

use crate::phone_common;

use api::ai::Ai;
use api::app;
use api::storage::Storage;
use axum::http::StatusCode;
use axum_test::TestServer;
use phone_common::{
    HEADER, JOB_KEY, Stubs, add_call, add_phone, add_session, enable_calling, phone_server_with,
    phone_state, sql, token,
};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::atomic::Ordering;

/// Runs the job as the `time-triggered` Lambda does: what it did.
async fn run_job(server: &TestServer) -> Value {
    let response = server
        .post("/v1/jobs/phone")
        .add_header("authorization", JOB_KEY)
        .await;
    response.assert_status_ok();
    response.json()
}

/// A server whose recordings bucket the test can fill.
async fn server(pool: &MySqlPool) -> (TestServer, Stubs, Storage) {
    let storage = Storage::memory();
    let bucket = storage.clone();
    let (server, stubs) =
        phone_server_with(pool.clone(), move |state| state.with_storage(bucket)).await;
    (server, stubs, storage)
}

/// A recorded call waiting for its AI note: its recording is `audio`, in
/// the bucket at `phone/100/2026/10/{id}-{kind}.mp3`.
async fn recorded(storage: &Storage, pool: &MySqlPool, id: i64, kind: &str, audio: &[u8]) {
    let key = format!("phone/100/2026/10/{id}-{kind}.mp3");
    storage
        .put_private("phone-recordings", &key, audio.to_vec(), "audio/mpeg")
        .await
        .unwrap();
    sql(
        pool,
        &format!(
            "UPDATE phone_calls SET recording_status = 'copied', recording_key = '{key}',
                    recording_telnyx_id = 'rec-{id}', telnyx_deleted_at = UTC_TIMESTAMP(),
                    transcript_status = 'pending'
              WHERE id = {id}"
        ),
    )
    .await;
}

/// A call's AI note state: (transcript status, attempts, AI note, deal note,
/// transcript).
async fn note_state(
    pool: &MySqlPool,
    id: i64,
) -> (String, i64, Option<String>, Option<i64>, Option<String>) {
    sqlx::query_as(
        "SELECT CAST(transcript_status AS CHAR), CAST(transcribe_attempts AS SIGNED), ai_note,
                CAST(note_id AS SIGNED), transcript
           FROM phone_calls WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

const NOTE: &str = "- **Project:** kitchen, white quartz\n- **Sink:** double";
/// What OpenAI answers for a voicemail, and the note the CRM's clean-up
/// makes of it (a "- Voicemail" heading, the label in bold).
const VOICEMAIL_REPLY: &str = "- Action: Customer asked for a call back about the quote";
const VOICEMAIL_NOTE: &str =
    "- Voicemail\n- **Action**: Customer asked for a call back about the quote";

/// A run's counts without the customer phone index's, which depend on the
/// customers there are (`the_job_keeps_the_phone_index_in_step` tests them).
fn without_index(mut run: Value) -> Value {
    let counts = run.as_object_mut().unwrap();
    assert!(counts.remove("phone_index_refreshed").is_some());
    assert!(counts.remove("phone_index_filled").is_some());
    run
}

#[test_db::test(seed)]
async fn the_job_writes_ai_notes_on_calls_and_their_deals(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    sql(&pool, "INSERT INTO customers (id, name, phone, company_id, source, sales_rep) VALUES (931001, 'Vera Customer', '317-555-0142', 100, 'leads', 101)").await;
    sql(&pool, "INSERT INTO deals (id, customer_id, title, status, list_id, position, user_id) VALUES (931101, 931001, 'Vera kitchen', 'New', 100, 0, 100)").await;
    // Rita's answered call with Vera, a voicemail nobody took, and Rita's
    // call that reached a mailbox greeting.
    add_call(&pool, 981_001, 100, Some(100), "inbound", "answered", "+13175550142", "2026-10-10 10:00:00",
        "customer_id = 931001, deal_id = 931101, answered_at = '2026-10-10 10:00:05', ended_at = '2026-10-10 10:05:05', duration_secs = 300").await;
    add_call(
        &pool,
        981_002,
        100,
        None,
        "inbound",
        "voicemail",
        "+13175550177",
        "2026-10-10 11:00:00",
        "ended_at = '2026-10-10 11:01:00'",
    )
    .await;
    add_call(&pool, 981_003, 100, Some(100), "outbound", "answered", "+13175550143", "2026-10-10 12:00:00",
        "answered_at = '2026-10-10 12:00:05', ended_at = UTC_TIMESTAMP() - INTERVAL 1 HOUR, duration_secs = 45").await;
    let (server, stubs, storage) = server(&pool).await;
    recorded(&storage, &pool, 981_001, "call", b"audio-981001").await;
    recorded(&storage, &pool, 981_002, "voicemail", b"audio-981002").await;
    recorded(&storage, &pool, 981_003, "call", b"audio-981003").await;
    stubs.add_transcript(
        b"audio-981001",
        "Hi, this is Rita from Acme. Hi Rita, we want white quartz for the kitchen.",
        &[
            ("A", "Hi, this is Rita from Acme."),
            ("B", "Hi Rita, we want white quartz for the kitchen."),
        ],
    );
    stubs.add_transcript(
        b"audio-981002",
        "Hi, this is Vera. Please call me back about the quote.",
        &[],
    );
    stubs.add_transcript(
        b"audio-981003",
        "The person you called is not available. Please leave a message after the tone.",
        &[],
    );
    stubs.set_chat_reply(&format!("  {NOTE}\n"));
    stubs.set_voicemail_reply(VOICEMAIL_REPLY);
    let run = run_job(&server).await;
    // Rita's call to the mailbox scores as an attempt.
    assert_eq!(
        without_index(run),
        json!({ "processed": 4, "phones_signed_out": 0, "sessions_expired": 0, "credentials_deleted": 0,
                "events_purged": 0, "calls_closed": 0, "recordings_copied": 0, "recordings_failed": 0,
                "telnyx_recordings_deleted": 0, "notes_written": 2, "notes_skipped": 1,
                "notes_failed": 0, "call_points": 1 })
    );
    let outcome: String = sqlx::query_scalar(
        "SELECT CAST(outcome AS CHAR) FROM phone_call_points WHERE provider = 'telnyx' AND provider_call_id = '981003'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(outcome, "attempt");
    // The answered call: the note on the call and on Vera's deal.
    let (status, attempts, ai_note, note_id, transcript) = note_state(&pool, 981_001).await;
    assert_eq!(
        (status.as_str(), attempts, ai_note.as_deref()),
        ("done", 1, Some(NOTE))
    );
    assert_eq!(
        transcript.as_deref(),
        Some(
            "Speaker A: Hi, this is Rita from Acme.\nSpeaker B: Hi Rita, we want white quartz for the kitchen."
        )
    );
    let note: (i64, i32, String, String) = sqlx::query_as(
        "SELECT CAST(deal_id AS SIGNED), company_id, content, created_by FROM deal_notes WHERE id = ?",
    )
    .bind(note_id.expect("a deal note"))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        note,
        (931_101, 100, NOTE.to_string(), "Granite Phone".to_string())
    );
    // The deal's rep and the customer's rep hear about it.
    let told: Vec<(i32, String, String)> = sqlx::query_as(
        "SELECT user_id, notification_type, actor_name FROM notifications WHERE deal_id = 931101 ORDER BY user_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        told,
        [
            (100, "note_added".into(), "Granite Phone".into()),
            (101, "note_added".into(), "Granite Phone".into())
        ]
    );
    // The voicemail: no deal, the note (tidied as the CRM tidies one) stays
    // on the call.
    let (status, _, ai_note, note_id, _) = note_state(&pool, 981_002).await;
    assert_eq!(
        (status.as_str(), ai_note.as_deref(), note_id),
        ("done", Some(VOICEMAIL_NOTE), None)
    );
    // The mailbox greeting: nothing to note, but the transcript is kept.
    let (status, _, ai_note, note_id, transcript) = note_state(&pool, 981_003).await;
    assert_eq!((status.as_str(), ai_note, note_id), ("skipped", None, None));
    assert!(transcript.unwrap().starts_with("The person you called"));
    // Speaker labels were asked for; the call prompt for the call, the
    // short voicemail prompt for the voicemail.
    assert!(
        stubs
            .transcript_requests()
            .iter()
            .all(|request| request["speaker_labels"] == true)
    );
    let chats = stubs.chats();
    assert_eq!(chats.len(), 2);
    let by_transcript = |start: &str| {
        chats
            .iter()
            .find(|chat| {
                chat["messages"][1]["content"]
                    .as_str()
                    .unwrap()
                    .starts_with(start)
            })
            .unwrap()
            .clone()
    };
    let call = by_transcript("Speaker A:");
    assert_eq!(call["model"], "gpt-6-luna");
    assert!(
        call["messages"][0]["content"]
            .as_str()
            .unwrap()
            .starts_with("Summarize this phone sales call")
    );
    assert!(call.get("max_completion_tokens").is_none());
    // A voicemail left for us: the inbound voicemail prompt.
    let voicemail = by_transcript("Hi, this is Vera");
    assert!(
        voicemail["messages"][0]["content"]
            .as_str()
            .unwrap()
            .starts_with("Summarize this voicemail a caller left")
    );
    assert_eq!(voicemail["max_completion_tokens"], 180);
    let logged: i64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS SIGNED) FROM ai_usage_log WHERE feature = 'phone_call_note'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(logged, 5);
    // Nothing is noted twice.
    let again = run_job(&server).await;
    assert_eq!(
        (&again["notes_written"], &again["processed"]),
        (&json!(0), &json!(0))
    );
    assert_eq!(stubs.chats().len(), 2);
    assert_eq!(stubs.uploads().len(), 3);
    let notes: i64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS SIGNED) FROM deal_notes WHERE deal_id = 931101",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(notes, 1);
}

#[test_db::test(seed)]
async fn failed_notes_are_retried_then_given_up(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    for id in [981_011, 981_012, 981_013] {
        add_call(&pool, id, 100, Some(100), "inbound", "answered", "+13175550142", "2026-10-10 10:00:00",
            "answered_at = '2026-10-10 10:00:05', ended_at = '2026-10-10 10:05:05', duration_secs = 300").await;
    }
    let (server, stubs, storage) = server(&pool).await;
    // AssemblyAI does not know this audio: an error.
    recorded(&storage, &pool, 981_011, "call", b"audio-unknown").await;
    // Transcribed, but OpenAI is down.
    recorded(&storage, &pool, 981_012, "call", b"audio-981012").await;
    stubs.add_transcript(b"audio-981012", "We want quartz.", &[]);
    // The recording is gone from the bucket.
    sql(&pool, "UPDATE phone_calls SET recording_status = 'copied', recording_key = 'phone/100/2026/10/981013-call.mp3', transcript_status = 'pending' WHERE id = 981013").await;
    stubs.chat_down.store(true, Ordering::SeqCst);
    stubs.set_chat_reply(NOTE);
    let run = run_job(&server).await;
    assert_eq!(
        (
            &run["notes_written"],
            &run["notes_skipped"],
            &run["notes_failed"]
        ),
        (&json!(0), &json!(0), &json!(1))
    );
    assert_eq!(note_state(&pool, 981_011).await.0, "pending");
    assert_eq!(note_state(&pool, 981_011).await.1, 1);
    let (status, attempts, _, _, transcript) = note_state(&pool, 981_012).await;
    assert_eq!(
        (status.as_str(), attempts, transcript.as_deref()),
        ("pending", 1, Some("We want quartz."))
    );
    assert_eq!(note_state(&pool, 981_013).await.0, "failed");
    // Within the claim's 15 minutes nothing is tried again.
    let uploads = stubs.uploads().len();
    run_job(&server).await;
    assert_eq!(stubs.uploads().len(), uploads);
    // Later, with OpenAI back: the kept transcript is noted (no new upload).
    sql(&pool, "UPDATE phone_calls SET transcribe_claimed_at = UTC_TIMESTAMP() - INTERVAL 16 MINUTE WHERE id IN (981011, 981012)").await;
    sql(
        &pool,
        "UPDATE phone_calls SET transcribe_attempts = 4 WHERE id = 981011",
    )
    .await;
    stubs.chat_down.store(false, Ordering::SeqCst);
    let run = run_job(&server).await;
    assert_eq!(
        (&run["notes_written"], &run["notes_failed"]),
        (&json!(1), &json!(1))
    );
    assert_eq!(stubs.uploads().len(), uploads + 1);
    let (status, attempts, ai_note, _, _) = note_state(&pool, 981_012).await;
    assert_eq!(
        (status.as_str(), attempts, ai_note.as_deref()),
        ("done", 2, Some(NOTE))
    );
    // The fifth failure gives the note up.
    assert_eq!(note_state(&pool, 981_011).await.0, "failed");
    assert_eq!(note_state(&pool, 981_011).await.1, 5);
}

#[test_db::test(seed)]
async fn without_openai_the_claim_is_given_back(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    add_call(&pool, 981_021, 100, Some(100), "inbound", "answered", "+13175550142", "2026-10-10 10:00:00",
        "answered_at = '2026-10-10 10:00:05', ended_at = '2026-10-10 10:05:05', duration_secs = 300").await;
    let (state, stubs) = phone_state(pool.clone()).await;
    let storage = Storage::memory();
    let server = TestServer::new(app(state
        .with_storage(storage.clone())
        .with_ai(Ai::disabled())));
    recorded(&storage, &pool, 981_021, "call", b"audio-981021").await;
    stubs.add_transcript(b"audio-981021", "We want quartz.", &[]);
    let run = run_job(&server).await;
    assert_eq!(run["processed"], 0);
    // Transcribed and kept; the attempt is not counted, so the next run
    // (once OpenAI is set up) can note it at once.
    let (status, attempts, ai_note, _, transcript) = note_state(&pool, 981_021).await;
    assert_eq!(
        (status.as_str(), attempts, ai_note, transcript.as_deref()),
        ("pending", 0, None, Some("We want quartz."))
    );
    let claimed: Option<String> = sqlx::query_scalar(
        "SELECT CAST(transcribe_claimed_at AS CHAR) FROM phone_calls WHERE id = 981021",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(claimed, None);
}

#[test_db::test(seed)]
async fn ai_notes_are_tidied_as_the_crm_tidies_them(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    add_call(&pool, 981_031, 100, Some(100), "inbound", "answered", "+13175550142", "2026-10-10 10:00:00",
        "answered_at = '2026-10-10 10:00:05', ended_at = '2026-10-10 10:05:05', duration_secs = 300").await;
    let (server, stubs, storage) = server(&pool).await;
    recorded(&storage, &pool, 981_031, "call", b"audio-981031").await;
    stubs.add_transcript(
        b"audio-981031",
        "We want white quartz. Budget is about $7,500.",
        &[
            ("A", "We want white quartz."),
            ("B", "Budget is about $7,500."),
        ],
    );
    // A Customer bullet, a bullet that says nothing and a sink negation go;
    // the budget the customer gave is added (as the CRM's
    // sanitizeCallNoteContent does it).
    stubs.set_chat_reply(
        "- Customer: Jane\n- Project: kitchen, white quartz\n- Color: not specified\n- Sink: double (not in the island)",
    );
    let run = run_job(&server).await;
    assert_eq!(run["notes_written"], 1);
    let (status, _, ai_note, _, _) = note_state(&pool, 981_031).await;
    assert_eq!(
        (status.as_str(), ai_note.as_deref()),
        (
            "done",
            Some(
                "- **Project**: kitchen, white quartz\n- **Sink**: double\n- **Budget**: $7,500 for quartz and installation"
            )
        )
    );
    // Nothing left after the clean-up: nothing to note.
    add_call(&pool, 981_032, 100, Some(100), "inbound", "answered", "+13175550142", "2026-10-10 11:00:00",
        "answered_at = '2026-10-10 11:00:05', ended_at = '2026-10-10 11:05:05', duration_secs = 300").await;
    recorded(&storage, &pool, 981_032, "call", b"audio-981032").await;
    stubs.add_transcript(b"audio-981032", "Hello?", &[("A", "Hello?")]);
    stubs.set_chat_reply("- Customer: Jane\n- Call type: Inbound\n- Color: N/A");
    let run = run_job(&server).await;
    assert_eq!(
        (&run["notes_written"], &run["notes_skipped"]),
        (&json!(0), &json!(1))
    );
    let (status, _, ai_note, note_id, _) = note_state(&pool, 981_032).await;
    assert_eq!((status.as_str(), ai_note, note_id), ("skipped", None, None));
}

/// The customer phone index's rows of these customers: (company, last 10
/// digits, customer), by customer and digits.
async fn index_rows(pool: &MySqlPool, ids: &[i64]) -> Vec<(i32, String, i64)> {
    let list = ids
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT company_id, last10, CAST(customer_id AS SIGNED) FROM customer_phone_index
          WHERE customer_id IN ({list}) ORDER BY customer_id, last10"
    )))
    .fetch_all(pool)
    .await
    .unwrap()
}

/// The customer each number is matched to (`POST /v1/phone/contacts/lookup`
/// with phone `n`'s token), in order.
async fn matched(server: &TestServer, n: u32, numbers: &[&str]) -> Vec<Option<i64>> {
    let body: Value = server
        .post("/v1/phone/contacts/lookup")
        .add_header(HEADER, token(n))
        .json(&json!({ "numbers": numbers }))
        .await
        .json();
    body["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|found| found["contact"]["id"].as_i64())
        .collect()
}

/// Customers of companies that are not deleted (what a first fill reads).
async fn company_customers(pool: &MySqlPool, company_id: Option<i32>) -> i64 {
    sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS SIGNED) FROM customers c
           JOIN company co ON co.id = c.company_id AND co.deleted_at IS NULL
          WHERE ? IS NULL OR c.company_id = ?",
    )
    .bind(company_id)
    .bind(company_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[test_db::test(seed)]
async fn the_job_keeps_the_phone_index_in_step_with_the_customers(pool: MySqlPool) {
    // The seed's own changes are long ago.
    sql(
        &pool,
        "UPDATE customers SET updated_at = '2026-01-01 00:00:00'",
    )
    .await;
    add_session(&pool, 131, 100, 100).await;
    let (server, _, _) = server(&pool).await;
    // Before the first fill, lookups read the customers' phones.
    sql(&pool, "INSERT INTO customers (id, name, phone, company_id, source) VALUES (931201, 'Before', '317-555-0161', 100, 'leads')").await;
    assert_eq!(
        matched(&server, 131, &["3175550161"]).await,
        [Some(931_201)]
    );
    // The first run fills every company's index (and takes changes from now
    // on).
    let run = run_job(&server).await;
    assert_eq!(
        (
            &run["phone_index_filled"],
            &run["phone_index_refreshed"],
            &run["processed"]
        ),
        (
            &json!(company_customers(&pool, None).await),
            &json!(0),
            &json!(0)
        )
    );
    // Alice's two phones, Carol's one (her empty phone 2 left out), company
    // 101's customer under 101; nothing for deleted Dan or the sample.
    assert_eq!(
        index_rows(&pool, &[1000, 1002, 1003, 2000, 990_000_021]).await,
        [
            (100, "3175550100".to_string(), 1000),
            (100, "3175550101".to_string(), 1000),
            (100, "3175550103".to_string(), 1002),
            (101, "4635550200".to_string(), 2000),
        ]
    );
    let built: i64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS SIGNED) FROM customer_phone_index_builds
          WHERE company_id IN (100, 101) AND built_at IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(built, 2);
    // From now on lookups read the index: a customer written as no CRM page
    // writes one (no new id, no updated_at) is not found ...
    sql(&pool, "INSERT INTO customers (id, name, phone, company_id, source) VALUES (931202, 'Unseen', '317-555-0162', 100, 'leads')").await;
    assert_eq!(
        matched(&server, 131, &["3175550161", "3175550162", "3175550100"]).await,
        [Some(931_201), None, Some(1000)]
    );
    // ... until it is edited. A new customer; Alice's phone 2 changes, Bob is
    // deleted and Erin moves to company 101.
    sql(
        &pool,
        "UPDATE customers SET name = 'Seen' WHERE id = 931202",
    )
    .await;
    let new_id = i64::try_from(
        sqlx::query(
            "INSERT INTO customers (name, phone, phone_2, company_id, source)
             VALUES ('Brand New', '(317) 555-0163', '317.555.0164', 100, 'leads')",
        )
        .execute(&pool)
        .await
        .unwrap()
        .last_insert_id(),
    )
    .unwrap();
    sql(
        &pool,
        "UPDATE customers SET phone_2 = '317-555-0165' WHERE id = 1000",
    )
    .await;
    sql(
        &pool,
        "UPDATE customers SET deleted_at = UTC_TIMESTAMP() WHERE id = 1001",
    )
    .await;
    sql(
        &pool,
        "UPDATE customers SET company_id = 101 WHERE id = 1004",
    )
    .await;
    // Before the job runs, a stale row names nobody wrongly: Alice's old
    // phone 2, deleted Bob and Erin (no longer company 100's) match no one.
    assert_eq!(
        matched(&server, 131, &["3175550101", "3175550102", "3175550105"]).await,
        [None, None, None]
    );
    let run = run_job(&server).await;
    assert_eq!(
        (&run["phone_index_refreshed"], &run["phone_index_filled"]),
        (&json!(5), &json!(0))
    );
    assert_eq!(
        index_rows(&pool, &[931_202, new_id, 1000, 1001, 1004]).await,
        [
            (100, "3175550100".to_string(), 1000),
            (100, "3175550165".to_string(), 1000),
            (101, "3175550105".to_string(), 1004),
            (100, "3175550162".to_string(), 931_202),
            (100, "3175550163".to_string(), new_id),
            (100, "3175550164".to_string(), new_id),
        ]
    );
    assert_eq!(
        matched(
            &server,
            131,
            &[
                "3175550162",
                "3175550163",
                "3175550164",
                "3175550165",
                "3175550101",
                "3175550102",
                "3175550105"
            ],
        )
        .await,
        [
            Some(931_202),
            Some(new_id),
            Some(new_id),
            Some(1000),
            None,
            None,
            None
        ]
    );
    // What changed in the last 2 minutes is read again (a write that
    // commits late is not missed) ...
    let run = run_job(&server).await;
    assert_eq!(run["phone_index_refreshed"], 5);
    // ... and once it is older, the job moves past it.
    sql(&pool, "UPDATE customers SET updated_at = UTC_TIMESTAMP() - INTERVAL 5 MINUTE WHERE id IN (931202, 1000, 1001, 1004)").await;
    sql(&pool, &format!("UPDATE customers SET created_date = UTC_TIMESTAMP() - INTERVAL 5 MINUTE, updated_at = UTC_TIMESTAMP() - INTERVAL 5 MINUTE WHERE id = {new_id}")).await;
    assert_eq!(run_job(&server).await["phone_index_refreshed"], 1);
    assert_eq!(run_job(&server).await["phone_index_refreshed"], 0);
}

#[test_db::test(seed)]
async fn a_big_company_is_filled_over_runs_and_read_as_before_until_then(pool: MySqlPool) {
    // 5,003 more customers for company 101 (463-555-0001 ...).
    let digits = "(SELECT 0 AS d UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3
                   UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7
                   UNION ALL SELECT 8 UNION ALL SELECT 9)";
    sql(
        &pool,
        &format!(
            "INSERT INTO customers (id, name, phone, company_id, source)
             SELECT 940000 + n, CONCAT('Bulk ', n), CONCAT('463-555-', LPAD(n MOD 10000, 4, '0')),
                    101, 'leads'
               FROM (SELECT a.d + 10 * b.d + 100 * c.d + 1000 * e.d + 1 AS n
                       FROM {digits} a, {digits} b, {digits} c, {digits} e) seq
              WHERE n <= 5003"
        ),
    )
    .await;
    add_session(&pool, 132, 106, 101).await;
    let (server, _, _) = server(&pool).await;
    // A run reads at most 5,000 customers: company 100 is filled, company
    // 101 has the rest to go.
    let run = run_job(&server).await;
    let company_100 = company_customers(&pool, Some(100)).await;
    assert_eq!(run["phone_index_filled"], company_100 + 5000);
    let state: Vec<(i32, i32, bool)> = sqlx::query_as(
        "SELECT company_id, done_id, built_at IS NOT NULL FROM customer_phone_index_builds
          WHERE company_id IN (100, 101) ORDER BY company_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!((state[1].0, state[1].1, state[1].2), (101, 944_999, false));
    assert!(state[0].2);
    // Until company 101's index is filled, its lookups read the phones: the
    // last customer, not indexed yet, is found (and 200's number is the
    // newest customer's with it).
    assert_eq!(
        matched(&server, 132, &["4635555003", "4635550200"]).await,
        [Some(945_003), Some(940_200)]
    );
    let run = run_job(&server).await;
    assert_eq!(run["phone_index_filled"], 4);
    // Filled: the index answers, and a customer it never took is not found.
    sql(&pool, "INSERT INTO customers (id, name, phone, company_id, source) VALUES (931301, 'Unseen', '463-555-9999', 101, 'leads')").await;
    assert_eq!(
        matched(&server, 132, &["4635555003", "4635550200", "4635559999"]).await,
        [Some(945_003), Some(940_200), None]
    );
    assert_eq!(run_job(&server).await["phone_index_filled"], 0);
}

/// A call (`CallSid` `v3:job-{id}`) whose recording copy waits for the
/// job: `extra` sets more columns.
async fn copy_pending(pool: &MySqlPool, id: i64, status: &str, recording: &str, extra: &str) {
    add_call(
        pool,
        id,
        100,
        Some(100),
        "inbound",
        status,
        "+13175550142",
        "2026-10-10 10:00:00",
        &format!(
            "ended_at = '2026-10-10 10:05:05', ai_notes = 0, recording_status = 'pending',
                  recording_telnyx_id = '{recording}', telnyx_call_sid = 'v3:job-{id}', {extra}"
        ),
    )
    .await;
}

/// Minutes ago, as SQL.
const TEN_MINUTES_AGO: &str = "updated_at = UTC_TIMESTAMP() - INTERVAL 10 MINUTE";

/// A call's recording state: (status, key, attempts, transcript status).
async fn copy_state(pool: &MySqlPool, id: i64) -> (String, Option<String>, i64, String) {
    sqlx::query_as(
        "SELECT CAST(recording_status AS CHAR), recording_key, CAST(recording_attempts AS SIGNED),
                CAST(transcript_status AS CHAR)
           FROM phone_calls WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[test_db::test(seed)]
async fn recording_copies_are_retried_then_given_up(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    // Failed once already; just started (by the webhook or another run: left
    // alone); failed four times and still missing on Telnyx; a voicemail
    // whose copy never finished; one out of attempts; and one that names a
    // recording of another call.
    copy_pending(
        &pool,
        981_101,
        "answered",
        "rec-1101",
        &format!("recording_attempts = 1, {TEN_MINUTES_AGO}"),
    )
    .await;
    copy_pending(
        &pool,
        981_102,
        "answered",
        "rec-1102",
        "recording_attempts = 1",
    )
    .await;
    copy_pending(
        &pool,
        981_103,
        "answered",
        "rec-gone",
        &format!("recording_attempts = 4, {TEN_MINUTES_AGO}"),
    )
    .await;
    copy_pending(
        &pool,
        981_104,
        "voicemail",
        "rec-1104",
        &format!("recording_attempts = 0, {TEN_MINUTES_AGO}"),
    )
    .await;
    copy_pending(
        &pool,
        981_105,
        "answered",
        "rec-1105",
        &format!("recording_attempts = 5, {TEN_MINUTES_AGO}"),
    )
    .await;
    copy_pending(
        &pool,
        981_106,
        "answered",
        "rec-1106",
        &format!("recording_attempts = 1, {TEN_MINUTES_AGO}"),
    )
    .await;
    let (server, stubs, storage) = server(&pool).await;
    for id in ["1101", "1102", "1104", "1105"] {
        let link = stubs.add_file(&format!("rec-{id}.mp3"), format!("mp3 {id}").as_bytes());
        stubs.add_recording(
            &format!("rec-{id}"),
            &link,
            61_000,
            &format!("v3:job-98{id}"),
        );
    }
    let link = stubs.add_file("rec-1106.mp3", b"mp3 1106");
    stubs.add_recording("rec-1106", &link, 61_000, "v3:another-call");
    let run = run_job(&server).await;
    assert_eq!(
        (&run["recordings_copied"], &run["recordings_failed"]),
        (&json!(2), &json!(3))
    );
    // Each copy counted as an attempt before it started.
    assert_eq!(
        copy_state(&pool, 981_101).await,
        (
            "copied".into(),
            Some("phone/100/2026/10/981101-call.mp3".into()),
            2,
            "none".into()
        )
    );
    assert_eq!(
        copy_state(&pool, 981_102).await,
        ("pending".into(), None, 1, "none".into())
    );
    assert_eq!(
        copy_state(&pool, 981_103).await,
        ("failed".into(), None, 5, "none".into())
    );
    // Not this call's recording: given up without a download or a delete.
    assert_eq!(
        copy_state(&pool, 981_106).await,
        ("failed".into(), None, 1, "none".into())
    );
    // A voicemail is always noted: its transcript is queued (and the
    // stand-in AssemblyAI does not know the audio, so it waits).
    let voicemail = copy_state(&pool, 981_104).await;
    assert_eq!(
        (
            voicemail.0.as_str(),
            voicemail.1.as_deref(),
            voicemail.3.as_str()
        ),
        (
            "copied",
            Some("phone/100/2026/10/981104-voicemail.mp3"),
            "pending"
        )
    );
    assert_eq!(copy_state(&pool, 981_105).await.0, "failed");
    assert_eq!(
        storage
            .get("phone/100/2026/10/981101-call.mp3")
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"mp3 1101"
    );
    // Telnyx's copies of the copied ones are deleted.
    let paths = stubs.telnyx_paths();
    for path in [
        "DELETE /recordings/rec-1101",
        "DELETE /recordings/rec-1104",
        "GET /recordings/rec-gone",
    ] {
        assert!(paths.contains(&path.to_string()), "{path}: {paths:?}");
    }
    assert!(
        !paths
            .iter()
            .any(|path| path.contains("rec-1102") || path.contains("rec-1105"))
    );
    assert!(!paths.contains(&"DELETE /recordings/rec-1106".to_string()));
}

#[test_db::test(seed)]
async fn telnyx_deletes_that_failed_are_retried_with_backoff(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    // Copied, but Telnyx's copy is still there: never tried, tried and failed
    // a minute ago (its first wait is 2 minutes), and tried three times, the
    // last 10 minutes ago (it waits 8).
    for (id, extra) in [
        (981_301, String::new()),
        (981_302, ", telnyx_delete_attempts = 1, telnyx_delete_tried_at = UTC_TIMESTAMP() - INTERVAL 1 MINUTE".to_string()),
        (981_303, ", telnyx_delete_attempts = 3, telnyx_delete_tried_at = UTC_TIMESTAMP() - INTERVAL 10 MINUTE".to_string()),
    ] {
        add_call(&pool, id, 100, Some(100), "inbound", "answered", "+13175550142", "2026-10-10 10:00:00",
            &format!("ended_at = '2026-10-10 10:05:05', recording_status = 'copied',
                      recording_key = 'phone/100/2026/10/{id}-call.mp3', recording_telnyx_id = 'rec-{id}'{extra}")).await;
    }
    // A signed-out phone whose credential delete failed a minute ago.
    add_phone(&pool, 140, 100, 100).await;
    sql(&pool, "UPDATE device_sessions SET revoked_at = UTC_TIMESTAMP(), revoked_reason = 'signed_out' WHERE id = 950140").await;
    sql(&pool, "UPDATE phone_devices SET revoked_at = UTC_TIMESTAMP(), revoke_reason = 'signed_out', push_token = NULL,
                credential_delete_attempts = 1, credential_delete_tried_at = UTC_TIMESTAMP() - INTERVAL 1 MINUTE
          WHERE id = 960140").await;
    let (server, stubs, _storage) = server(&pool).await;
    // Telnyx fails the deletes: each try is counted.
    stubs.fail_deletes.store(true, Ordering::SeqCst);
    let run = run_job(&server).await;
    assert_eq!(run["telnyx_recordings_deleted"], 0);
    assert_eq!(
        stubs.telnyx_paths(),
        [
            "DELETE /recordings/rec-981301",
            "DELETE /recordings/rec-981303"
        ]
    );
    let tries: Vec<i64> = sqlx::query_scalar(
        "SELECT CAST(telnyx_delete_attempts AS SIGNED) FROM phone_calls WHERE id BETWEEN 981301 AND 981303 ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(tries, [1, 1, 4]);
    // Later, with Telnyx back: the ones due are deleted, never tried first.
    stubs.clear_telnyx();
    stubs.fail_deletes.store(false, Ordering::SeqCst);
    sql(&pool, "UPDATE phone_calls SET telnyx_delete_tried_at = UTC_TIMESTAMP() - INTERVAL 1 DAY WHERE id BETWEEN 981301 AND 981303").await;
    sql(&pool, "UPDATE phone_devices SET credential_delete_tried_at = UTC_TIMESTAMP() - INTERVAL 1 DAY WHERE id = 960140").await;
    let run = run_job(&server).await;
    assert_eq!(
        (
            &run["telnyx_recordings_deleted"],
            &run["credentials_deleted"]
        ),
        (&json!(3), &json!(1))
    );
    let left: i64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS SIGNED) FROM phone_calls WHERE id BETWEEN 981301 AND 981303 AND telnyx_deleted_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(left, 0);
}

#[test_db::test(seed)]
async fn the_job_closes_calls_left_open_and_ends_signed_out_browser_phones(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    // Telnyx never reported their end: an answered incoming call and a
    // ringing one 7 hours old, an outgoing one, a voicemail left open, and a
    // live call of an hour ago (left alone).
    let seven_hours = "started_at = UTC_TIMESTAMP() - INTERVAL 7 HOUR";
    add_call(
        &pool,
        981_401,
        100,
        Some(100),
        "inbound",
        "in_progress",
        "+13175550142",
        "2026-10-10 10:00:00",
        &format!("{seven_hours}, answered_at = UTC_TIMESTAMP() - INTERVAL 7 HOUR"),
    )
    .await;
    add_call(
        &pool,
        981_402,
        100,
        None,
        "inbound",
        "ringing",
        "+13175550142",
        "2026-10-10 10:00:00",
        seven_hours,
    )
    .await;
    add_call(
        &pool,
        981_403,
        100,
        Some(100),
        "outbound",
        "ringing",
        "+13175550142",
        "2026-10-10 10:00:00",
        seven_hours,
    )
    .await;
    add_call(
        &pool,
        981_404,
        100,
        None,
        "inbound",
        "voicemail",
        "+13175550142",
        "2026-10-10 10:00:00",
        seven_hours,
    )
    .await;
    add_call(&pool, 981_405, 100, Some(100), "inbound", "in_progress", "+13175550142", "2026-10-10 10:00:00",
        "started_at = UTC_TIMESTAMP() - INTERVAL 1 HOUR, answered_at = UTC_TIMESTAMP() - INTERVAL 1 HOUR").await;
    // Phones: one idle 31 days and one past its 12 months stay signed in; a
    // browser phone whose CRM sign-in was signed out is ended.
    add_phone(&pool, 141, 100, 100).await;
    add_phone(&pool, 142, 100, 100).await;
    add_phone(&pool, 143, 100, 100).await;
    sql(&pool, "UPDATE device_sessions SET last_seen_at = UTC_TIMESTAMP() - INTERVAL 31 DAY WHERE id = 950141").await;
    sql(&pool, "UPDATE device_sessions SET expires_at = UTC_TIMESTAMP() - INTERVAL 1 MINUTE WHERE id = 950142").await;
    sql(&pool, "UPDATE device_sessions SET platform = 'web', web_session_id = 'bbbbbbbb-0000-4000-8000-0000000d0100' WHERE id = 950143").await;
    let (server, stubs, _storage) = server(&pool).await;
    let run = run_job(&server).await;
    assert_eq!(
        (&run["calls_closed"], &run["sessions_expired"]),
        (&json!(4), &json!(1))
    );
    let calls: Vec<(String, bool, Option<i32>)> = sqlx::query_as(
        "SELECT CAST(status AS CHAR), ended_at IS NOT NULL, duration_secs FROM phone_calls
          WHERE id BETWEEN 981401 AND 981405 ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        calls,
        [
            ("answered".into(), true, None),
            ("missed".into(), true, None),
            ("failed".into(), true, None),
            ("voicemail".into(), true, None),
            ("in_progress".into(), false, None),
        ]
    );
    let sessions: Vec<(i64, Option<String>)> = sqlx::query_as(
        "SELECT CAST(id AS SIGNED), revoked_reason FROM device_sessions WHERE id BETWEEN 950141 AND 950143 ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        sessions,
        [
            (950_141, None),
            (950_142, None),
            (950_143, Some("expired".into()))
        ]
    );
    // Its credential is deleted and push token dropped.
    let paths = stubs.telnyx_paths();
    assert_eq!(paths, ["DELETE /telephony_credentials/cred-143"]);
    let tokens: i64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS SIGNED) FROM phone_devices WHERE id = 960143 AND push_token IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(tokens, 0);
}

/// An outbound call Rita placed and the customer answered, ended an hour
/// ago: `extra` sets more columns.
async fn answered_out(pool: &MySqlPool, id: i64, talk: i32, extra: &str) {
    add_call(
        pool,
        id,
        100,
        Some(100),
        "outbound",
        "answered",
        "+13175550142",
        "2026-10-10 10:00:00",
        &format!(
            "answered_at = '2026-10-10 10:00:05', ended_at = UTC_TIMESTAMP() - INTERVAL 1 HOUR,
                  duration_secs = {talk}, our_number = '+13175550151', {extra}"
        ),
    )
    .await;
}

/// A call points row: (provider, call id, user, customer digits, talk
/// seconds, outcome, classified).
type PointsRow = (
    String,
    String,
    Option<i32>,
    Option<String>,
    i32,
    String,
    bool,
);

#[test_db::test(seed)]
async fn answered_outbound_calls_earn_call_points(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    sql(
        &pool,
        "UPDATE activity_points_sources SET counts_from = '2026-01-01' WHERE source = 'calls'",
    )
    .await;
    answered_out(&pool, 981_201, 5, "ai_notes = 1").await;
    answered_out(
        &pool,
        981_202,
        120,
        "transcript_status = 'done', transcript = 'Hello? Yes, we want the white quartz.'",
    )
    .await;
    answered_out(&pool, 981_203, 60, "transcript_status = 'skipped', transcript = 'Please leave a message after the tone. Hi, this is Rita with Acme about your quote.'").await;
    // Its transcript is still coming (not this job's to make: no recording).
    answered_out(&pool, 981_204, 95, "transcript_status = 'pending'").await;
    // Never recorded.
    answered_out(&pool, 981_205, 95, "ai_notes = 0").await;
    // Left out: a test call, an incoming call, an unanswered one, nobody's,
    // and one from before points counted.
    answered_out(
        &pool,
        981_206,
        95,
        "other_number = '+13175550151', other_last10 = '3175550151'",
    )
    .await;
    add_call(
        &pool,
        981_207,
        100,
        Some(100),
        "inbound",
        "answered",
        "+13175550142",
        "2026-10-10 10:00:00",
        "answered_at = '2026-10-10 10:00:05', ended_at = '2026-10-10 10:10:00', duration_secs = 95",
    )
    .await;
    add_call(
        &pool,
        981_208,
        100,
        Some(100),
        "outbound",
        "no_answer",
        "+13175550142",
        "2026-10-10 10:00:00",
        "ended_at = '2026-10-10 10:01:00'",
    )
    .await;
    answered_out(&pool, 981_209, 95, "user_id = NULL").await;
    answered_out(&pool, 981_210, 95, "started_at = '2025-12-01 10:00:00'").await;
    // An emergency call (no number to score), and one that ended 4 days ago.
    answered_out(
        &pool,
        981_211,
        95,
        "other_number = '911', other_last10 = ''",
    )
    .await;
    answered_out(
        &pool,
        981_212,
        95,
        "ended_at = UTC_TIMESTAMP() - INTERVAL 4 DAY",
    )
    .await;
    let (server, _stubs, _storage) = server(&pool).await;
    let run = run_job(&server).await;
    assert_eq!(run["call_points"], 5);
    let points = || async {
        let rows: Vec<PointsRow> = sqlx::query_as(
            "SELECT CAST(provider AS CHAR), provider_call_id, user_id, customer_digits, talk_seconds,
                    CAST(outcome AS CHAR), classified_at IS NOT NULL
               FROM phone_call_points WHERE company_id = 100 AND provider = 'telnyx'
              ORDER BY provider_call_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        rows
    };
    let row = |id: &str, talk: i32, outcome: &str, classified: bool| {
        (
            "telnyx".to_string(),
            id.to_string(),
            Some(100),
            Some("3175550142".to_string()),
            talk,
            outcome.to_string(),
            classified,
        )
    };
    assert_eq!(
        points().await,
        [
            row("981201", 5, "attempt", true),
            row("981202", 120, "conversation", true),
            row("981203", 60, "voicemail", true),
            row("981204", 95, "pending", false),
            row("981205", 95, "attempt", true),
        ]
    );
    // Once its transcript is in, the pending call is classified; the others
    // are never looked at again.
    sql(&pool, "UPDATE phone_calls SET transcript_status = 'done', transcript = 'Hi Rita! Yes, Saturday works for the measure.' WHERE id = 981204").await;
    let run = run_job(&server).await;
    assert_eq!(run["call_points"], 1);
    assert_eq!(points().await[3], row("981204", 95, "conversation", true));
    assert_eq!(run_job(&server).await["call_points"], 0);
}

#[test_db::test(seed)]
async fn the_job_signs_out_users_who_left_and_cleans_up(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    enable_calling(&pool, 101, "").await;
    // Rita stays; Vic is no employee; Gus is deleted; Mark moved to company
    // 101; Sam is a super admin (position 9) and stays.
    add_phone(&pool, 131, 100, 100).await;
    add_phone(&pool, 132, 104, 100).await;
    add_phone(&pool, 133, 105, 100).await;
    add_phone(&pool, 134, 101, 100).await;
    sql(&pool, "UPDATE users SET company_id = 101 WHERE id = 101").await;
    add_phone(&pool, 135, 103, 101).await;
    // Signed out earlier, but Telnyx failed to delete its credential.
    add_phone(&pool, 136, 100, 100).await;
    sql(&pool, "UPDATE device_sessions SET revoked_at = UTC_TIMESTAMP(), revoked_reason = 'signed_out' WHERE id = 950136").await;
    sql(&pool, "UPDATE phone_devices SET revoked_at = UTC_TIMESTAMP(), revoke_reason = 'signed_out', push_token = NULL WHERE id = 960136").await;
    // Callbacks from 31 and 29 days ago.
    sql(&pool, "INSERT INTO phone_call_events (company_id, source, event_type, dedupe_key, created_at) VALUES
                (100, 'texml', 'inbound', REPEAT('a', 64), UTC_TIMESTAMP() - INTERVAL 31 DAY),
                (100, 'texml', 'inbound', REPEAT('b', 64), UTC_TIMESTAMP() - INTERVAL 29 DAY)").await;
    let (server, stubs) = phone_server_with(pool.clone(), |state| state).await;
    // The job key is required.
    server
        .post("/v1/jobs/phone")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    server
        .post("/v1/jobs/phone")
        .add_header("authorization", "wrong")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    let run = run_job(&server).await;
    assert_eq!(
        (
            &run["phones_signed_out"],
            &run["credentials_deleted"],
            &run["events_purged"],
            &run["processed"]
        ),
        (&json!(3), &json!(1), &json!(1), &json!(4))
    );
    let sessions: Vec<(i64, Option<String>)> = sqlx::query_as(
        "SELECT CAST(id AS SIGNED), revoked_reason FROM device_sessions WHERE id BETWEEN 950131 AND 950136 ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        sessions,
        [
            (950_131, None),
            (950_132, Some("user_removed".into())),
            (950_133, Some("user_removed".into())),
            (950_134, Some("user_removed".into())),
            (950_135, None),
            (950_136, Some("signed_out".into())),
        ]
    );
    assert_eq!(
        stubs.telnyx_paths(),
        [
            "DELETE /telephony_credentials/cred-132",
            "DELETE /telephony_credentials/cred-133",
            "DELETE /telephony_credentials/cred-134",
            "DELETE /telephony_credentials/cred-136",
        ]
    );
    let told: Vec<(String, String)> = stubs
        .pushes()
        .iter()
        .map(|push| {
            (
                push["token"].as_str().unwrap().to_string(),
                push["data"]["type"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        told,
        [
            ("push-132".to_string(), "revoked".to_string()),
            ("push-133".to_string(), "revoked".to_string()),
            ("push-134".to_string(), "revoked".to_string()),
        ]
    );
    let deleted: i64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS SIGNED) FROM phone_devices
          WHERE id BETWEEN 960131 AND 960136 AND credential_deleted_at IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(deleted, 4);
    let events: i64 = sqlx::query_scalar("SELECT CAST(COUNT(*) AS SIGNED) FROM phone_call_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(events, 1);
    // Done: a second run has nothing left.
    stubs.clear_telnyx();
    let again = run_job(&server).await;
    assert_eq!(again["processed"], 0);
    assert!(stubs.telnyx().is_empty());
}

#[test_db::test(seed)]
async fn with_ai_notes_off_answered_outgoing_calls_are_still_scored(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    sql(
        &pool,
        "UPDATE activity_points_sources SET counts_from = '2026-01-01' WHERE source = 'calls'",
    )
    .await;
    sql(&pool, "INSERT INTO customers (id, name, phone, company_id, source, sales_rep) VALUES (931201, 'Quinn Talker', '317-555-0142', 100, 'leads', 100)").await;
    sql(&pool, "INSERT INTO deals (id, customer_id, title, status, list_id, position, user_id) VALUES (931301, 931201, 'Quinn kitchen', 'New', 100, 0, 100)").await;
    // Rita has AI notes off; two recorded calls with Quinn, 45 and 5 seconds.
    let extra = "ai_notes = 0, customer_id = 931201, deal_id = 931301";
    answered_out(&pool, 981_301, 45, extra).await;
    answered_out(&pool, 981_302, 5, extra).await;
    let (server, stubs, storage) = server(&pool).await;
    recorded(&storage, &pool, 981_301, "call", b"audio-981301").await;
    recorded(&storage, &pool, 981_302, "call", b"audio-981302").await;
    stubs.add_transcript(
        b"audio-981301",
        "Hello this is Rita. Oh hi Rita. How about Thursday? Thursday works.",
        &[
            ("A", "Hello this is Rita from Acme Stone"),
            ("B", "Oh hi Rita, yes we want the white quartz"),
            ("A", "Great, how about Thursday morning to measure"),
            ("B", "Thursday works great for me, thank you so much"),
        ],
    );
    run_job(&server).await;
    // Transcribed for the call points only: no AI note, nothing on the deal.
    let (status, _, note, note_id, transcript) = note_state(&pool, 981_301).await;
    assert_eq!((status.as_str(), note, note_id), ("none", None, None));
    assert!(
        transcript
            .unwrap()
            .contains("Speaker B: Thursday works great")
    );
    assert!(stubs.chats().is_empty());
    let deal_notes: i64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS SIGNED) FROM deal_notes WHERE deal_id = 931301",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(deal_notes, 0);
    // 5 seconds is an attempt whatever was said: not transcribed at all.
    assert_eq!(note_state(&pool, 981_302).await.0, "none");
    assert_eq!(stubs.uploads(), [b"audio-981301".to_vec()]);
    let points: Vec<(String, String)> = sqlx::query_as(
        "SELECT provider_call_id, CAST(outcome AS CHAR) FROM phone_call_points
          WHERE provider = 'telnyx' ORDER BY provider_call_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        points,
        [
            ("981301".to_string(), "conversation".to_string()),
            ("981302".to_string(), "attempt".to_string())
        ]
    );
    // The CRM's call check heard a conversation: the deal moved on.
    let list: i32 = sqlx::query_scalar("SELECT list_id FROM deals WHERE id = 931301")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(list, 101);
}
