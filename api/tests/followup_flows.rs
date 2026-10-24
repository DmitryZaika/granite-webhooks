//! SMS / email flows: sending due steps (`followups::processor`) and the
//! customer and thread flow routes, against stand-ins for the `webhooks`
//! Lambda (texts and mail), `CloudTalk` (calls) and `OpenAI`.

mod common;

use api::ai::Ai;
use api::app;
use api::followups::dates::{Instant, parse_iso};
use api::followups::processor::{FollowupRun, process_due_followups_at};
use api::state::AppState;
use api::storage::Storage;
use api::webhooks_client::{OutgoingEmail, OutgoingSms, WebhooksClient};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

// Acme (100) texts through CloudTalk; Rita (100) is agent 5001 and the sales
// rep of Paula (9600, 317-555-0177, paula@example.com). Flows are Rita's:
// 9700 a template step then one a day later, 9701 a text then an email
// with no delay, 9702 a step with a variable only the rep can fill, 9703 an
// AI step, 9704 a first step a day out, 9705 an AI follow-up Rita approved
// (two texts, two days apart).

const FIXTURE: &str = "
UPDATE company SET cloudtalk_access_key = 'ct-key', cloudtalk_access_secret = 'ct-secret' WHERE id = 100;
UPDATE users SET cloudtalk_agent_id = '5001', cloudtalk_phone_number = '3175550000' WHERE id = 100;
INSERT INTO customers (id, name, phone, company_id, source, sales_rep, created_date)
  VALUES (9600, 'Paula Prospect', '317-555-0177', 100, 'leads', 100, '2026-09-01 08:00:00');
INSERT INTO customers_emails (id, customer_id, email, created_at, created_by)
  VALUES (9601, 9600, 'paula@example.com', '2026-09-01 08:00:00', 100);
UPDATE customers SET email_id = 9601 WHERE id = 9600;
INSERT INTO sms_flows (id, company_id, user_id, name, created_at) VALUES
  (9700, 100, 100, 'Two texts', '2026-09-01 00:00:00'),
  (9701, 100, 100, 'Text then email', '2026-09-01 00:00:00'),
  (9702, 100, 100, 'Gate code', '2026-09-01 00:00:00'),
  (9703, 100, 100, 'Generated with AI', '2026-09-01 00:00:00'),
  (9704, 100, 100, 'Tomorrow', '2026-09-01 00:00:00'),
  (9705, 100, 100, 'Generated with AI', '2026-09-01 00:00:00');
INSERT INTO sms_flow_steps (flow_id, position, channel, template_id, custom_text, subject, delay_minutes) VALUES
  (9700, 1, 'sms', NULL, 'Hi {{customer.first_name}}, any questions about the quote?', NULL, 0),
  (9700, 2, 'sms', NULL, 'Second', NULL, 1440),
  (9701, 1, 'sms', NULL, 'One', NULL, 0),
  (9701, 2, 'email', NULL, 'Two', 'Your quote', 0),
  (9702, 1, 'sms', NULL, 'The gate code is {{gate code}}', NULL, 0),
  (9703, 1, 'sms', NULL, '__GR_AI_FOLLOWUP__', NULL, 0),
  (9704, 1, 'sms', NULL, 'Tomorrow', NULL, 1440),
  (9705, 1, 'sms', NULL, 'Want me to resend your quote so it is easy to find?', NULL, 0),
  (9705, 2, 'sms', NULL, 'Is the project still on for this fall?', NULL, 2880);
";

/// Wednesday noon in the store's time zone: both send windows are open.
fn noon() -> Instant {
    parse_iso("2026-10-14T16:00:00Z").unwrap()
}

#[derive(Clone, Default)]
struct Fakes {
    pool: Option<MySqlPool>,
    texts: Arc<Mutex<Vec<OutgoingSms>>>,
    emails: Arc<Mutex<Vec<OutgoingEmail>>>,
    /// The error the next texts get (none: sent).
    text_error: Arc<Mutex<Option<&'static str>>>,
    calls: Arc<Mutex<Vec<Value>>>,
    replies: Arc<Mutex<VecDeque<Value>>>,
    /// What the model was asked.
    prompts: Arc<Mutex<Vec<Value>>>,
}

async fn send_text(
    State(fakes): State<Fakes>,
    Json(sms): Json<OutgoingSms>,
) -> (StatusCode, Json<Value>) {
    fakes.texts.lock().unwrap().push(sms.clone());
    if let Some(error) = *fakes.text_error.lock().unwrap() {
        return (StatusCode::BAD_GATEWAY, Json(json!({ "error": error })));
    }
    let id = sqlx::query(
        "INSERT INTO cloudtalk_sms (recipient, text, direction, status, agent, sender_user_id, company_id,
                                    recipient10, phone_digits, is_automated)
         VALUES (?, ?, 'outbound', 'sent', ?, ?, ?, ?, ?, ?)",
    )
    .bind(&sms.phone_digits)
    .bind(&sms.text)
    .bind(&sms.agent_id)
    .bind(sms.user_id)
    .bind(sms.company_id)
    .bind(&sms.phone_digits)
    .bind(&sms.phone_digits)
    .bind(sms.is_automated)
    .execute(fakes.pool.as_ref().unwrap())
    .await
    .unwrap()
    .last_insert_id();
    (StatusCode::OK, Json(json!({ "ids": [id] })))
}

async fn send_email(State(fakes): State<Fakes>, Json(email): Json<OutgoingEmail>) -> Json<Value> {
    let mut sent = fakes.emails.lock().unwrap();
    sent.push(email);
    Json(json!({ "message_id": format!("flow-{}@ses.test", sent.len()) }))
}

async fn list_calls(State(fakes): State<Fakes>) -> Json<Value> {
    let calls = fakes.calls.lock().unwrap().clone();
    Json(json!({ "responseData": { "data": calls } }))
}

async fn complete(State(fakes): State<Fakes>, Json(request): Json<Value>) -> Json<Value> {
    fakes.prompts.lock().unwrap().push(request);
    let content = fakes
        .replies
        .lock()
        .unwrap()
        .pop_front()
        .expect("a scripted reply");
    Json(json!({
        "model": "gpt-6.1-sol-2026",
        "choices": [{ "message": { "content": content.to_string() }, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 900, "completion_tokens": 40 }
    }))
}

struct Setup {
    pool: MySqlPool,
    state: AppState,
    server: TestServer,
    fakes: Fakes,
}

async fn setup(pool: &MySqlPool) -> Setup {
    sqlx::raw_sql(FIXTURE).execute(pool).await.unwrap();
    let fakes = Fakes {
        pool: Some(pool.clone()),
        ..Fakes::default()
    };
    let router = Router::new()
        .route("/sms/send", post(send_text))
        .route("/email/send", post(send_email))
        .route("/calls/index.json", get(list_calls))
        .route("/v1/chat/completions", post(complete))
        .with_state(fakes.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_storage(Storage::memory())
        .with_cloudtalk_base(&base)
        .with_ai(Ai::new(
            Some("sk-test"),
            &format!("{base}/v1/chat/completions"),
        ))
        .with_webhooks(WebhooksClient::new(Some(&base), Some("webhooks-key")));
    Setup {
        pool: pool.clone(),
        server: TestServer::new(app(state.clone())),
        state,
        fakes,
    }
}

#[derive(sqlx::FromRow, Debug, PartialEq)]
struct Enrollment {
    status: String,
    current_step_position: i32,
    attempt_count: i32,
    error_message: Option<String>,
    next_send_at: Option<chrono::NaiveDateTime>,
    customer_id: Option<i32>,
}

impl Setup {
    /// Paula's run of the flow, due since the start of the month.
    async fn enroll(&self, flow_id: i32, customer_id: Option<i32>) -> i32 {
        let id = sqlx::query(
            "INSERT INTO sms_flow_enrollments (flow_id, company_id, customer_phone_digits, customer_email, customer_id,
                                               user_id, status, current_step_position, started_at, anchor_at, next_send_at)
             VALUES (?, 100, '3175550177', 'paula@example.com', ?, 100, 'active', 1,
                     '2026-10-01 12:00:00', '2026-10-01 12:00:00', '2026-10-01 12:00:00')",
        )
        .bind(flow_id)
        .bind(customer_id)
        .execute(&self.pool)
        .await
        .unwrap()
        .last_insert_id();
        i32::try_from(id).unwrap()
    }

    async fn enrollment(&self, id: i32) -> Enrollment {
        sqlx::query_as(
            "SELECT CAST(status AS CHAR) AS status, current_step_position, attempt_count, error_message,
                    CAST(next_send_at AS DATETIME) AS next_send_at, customer_id
               FROM sms_flow_enrollments WHERE id = ?",
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    async fn make_due(&self, id: i32) {
        sqlx::query(
            "UPDATE sms_flow_enrollments SET next_send_at = '2026-10-01 12:00:00' WHERE id = ?",
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    async fn run_at(&self, at: Instant) -> FollowupRun {
        process_due_followups_at(&self.state, Some(at))
            .await
            .unwrap()
    }

    fn texts(&self) -> Vec<OutgoingSms> {
        self.fakes.texts.lock().unwrap().clone()
    }

    fn emails(&self) -> Vec<OutgoingEmail> {
        self.fakes.emails.lock().unwrap().clone()
    }
}

fn naive(at: &str) -> chrono::NaiveDateTime {
    chrono::NaiveDateTime::parse_from_str(at, "%Y-%m-%d %H:%M:%S").unwrap()
}

#[test_db::test(seed)]
async fn a_template_step_is_filled_sent_and_the_next_one_scheduled(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let id = setup.enroll(9700, None).await;

    let run = setup.run_at(noon()).await;
    assert_eq!(
        run,
        FollowupRun {
            processed: 1,
            sent: 1,
            failed: 0,
            stopped: 0
        }
    );
    let texts = setup.texts();
    assert_eq!(texts.len(), 1);
    assert_eq!(texts[0].text, "Hi Paula, any questions about the quote?");
    assert_eq!(texts[0].phone_digits, "3175550177");
    assert_eq!(
        (
            texts[0].company_id,
            texts[0].user_id,
            texts[0].agent_id.as_str()
        ),
        (100, 100, "5001")
    );
    assert!(texts[0].is_automated);
    assert_eq!(
        texts[0].idempotency_key.as_deref(),
        Some(format!("sms-flow-{id}-1").as_str())
    );

    // The customer was found by phone for the variables and kept.
    let after = setup.enrollment(id).await;
    assert_eq!(
        after,
        Enrollment {
            status: "active".into(),
            current_step_position: 2,
            attempt_count: 0,
            error_message: None,
            next_send_at: Some(naive("2026-10-15 16:00:00")),
            customer_id: Some(9600),
        }
    );
    // Not due again until tomorrow.
    assert_eq!(setup.run_at(noon()).await.processed, 0);
}

#[test_db::test(seed)]
async fn steps_without_a_delay_go_out_in_the_same_run(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let id = setup.enroll(9701, Some(9600)).await;

    let run = setup.run_at(noon()).await;
    assert_eq!((run.processed, run.sent), (1, 2));
    assert_eq!(setup.texts()[0].text, "One");
    let emails = setup.emails();
    assert_eq!(emails.len(), 1);
    assert_eq!(emails[0].to, ["paula@example.com"]);
    assert_eq!(emails[0].subject, "Your quote");
    assert!(emails[0].body.contains("Two"));
    let after = setup.enrollment(id).await;
    assert_eq!(
        (after.status.as_str(), after.next_send_at),
        ("completed", None)
    );
    let automated: (i8, Option<i32>) = sqlx::query_as(
        "SELECT is_automated, deal_id FROM emails WHERE sender_user_id = 100 AND subject = 'Your quote'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(automated, (1, None));
}

#[test_db::test(seed)]
async fn a_text_from_the_customer_since_the_start_stops_the_run(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let id = setup.enroll(9700, Some(9600)).await;
    sqlx::query(
        "INSERT INTO cloudtalk_sms (sender, recipient, text, direction, status, company_id, created_date)
         VALUES ('3175550177', '3175550000', 'Not now', 'inbound', 'received', 100, '2026-10-02 09:00:00')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let run = setup.run_at(noon()).await;
    assert_eq!((run.stopped, run.sent), (1, 0));
    assert!(setup.texts().is_empty());
    let after = setup.enrollment(id).await;
    assert_eq!(
        (after.status.as_str(), after.next_send_at),
        ("stopped_by_reply", None)
    );
}

#[test_db::test(seed)]
async fn a_call_from_the_customer_since_the_start_stops_the_run(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let id = setup.enroll(9700, Some(9600)).await;
    setup.fakes.calls.lock().unwrap().extend([
        // Before the run started, and an outgoing call: neither counts.
        json!({ "Cdr": { "id": "1", "type": "incoming", "started_at": "2026-09-30T10:00:00.000Z" } }),
        json!({ "Cdr": { "id": "2", "type": "outgoing", "started_at": "2026-10-03T10:00:00.000Z" } }),
    ]);
    assert_eq!(setup.run_at(noon()).await.sent, 1);

    setup.make_due(id).await;
    setup
        .fakes
        .calls
        .lock()
        .unwrap()
        .push(json!({ "Cdr": { "id": "3", "type": "incoming", "started_at": "2026-10-05T10:00:00.000Z" } }));
    assert_eq!(setup.run_at(noon()).await.stopped, 1);
    assert_eq!(setup.texts().len(), 1);
    assert_eq!(setup.enrollment(id).await.status, "stopped_by_reply");
}

#[test_db::test(seed)]
async fn outside_the_send_window_the_step_waits_for_it(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let id = setup.enroll(9700, Some(9600)).await;

    // 2 AM in the store: texts start at 9.
    let run = setup
        .run_at(parse_iso("2026-10-14T06:00:00Z").unwrap())
        .await;
    assert_eq!((run.processed, run.sent), (1, 0));
    assert!(setup.texts().is_empty());
    let after = setup.enrollment(id).await;
    assert_eq!(after.status, "active");
    assert_eq!(after.current_step_position, 1);
    assert_eq!(after.next_send_at, Some(naive("2026-10-14 13:00:00")));
}

#[test_db::test(seed)]
async fn a_variable_only_the_rep_can_fill_fails_the_run(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let id = setup.enroll(9702, Some(9600)).await;

    let run = setup.run_at(noon()).await;
    assert_eq!((run.failed, run.sent), (1, 0));
    assert!(setup.texts().is_empty());
    let after = setup.enrollment(id).await;
    assert_eq!(
        (after.status.as_str(), after.error_message.as_deref()),
        ("failed", Some("unfilled_variables"))
    );
}

#[test_db::test(seed)]
async fn a_refused_text_is_retried_twice_then_fails_the_run(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let id = setup.enroll(9700, Some(9600)).await;
    *setup.fakes.text_error.lock().unwrap() = Some("cloudtalk_rejected");

    for attempt in 1..=2 {
        let run = setup.run_at(noon()).await;
        assert_eq!((run.sent, run.failed), (0, 0));
        let after = setup.enrollment(id).await;
        assert_eq!(
            (after.status.as_str(), after.attempt_count),
            ("active", attempt)
        );
        // The claim keeps it out of the next runs for 20 minutes.
        assert!(after.next_send_at.unwrap() > chrono::Utc::now().naive_utc());
        setup.make_due(id).await;
    }
    let run = setup.run_at(noon()).await;
    assert_eq!(run.failed, 1);
    let after = setup.enrollment(id).await;
    assert_eq!(
        (after.status.as_str(), after.error_message.as_deref()),
        ("failed", Some("cloudtalk_rejected"))
    );
    // Every retry asked the provider to drop a repeat of the same step.
    let keys: Vec<_> = setup
        .texts()
        .into_iter()
        .map(|t| t.idempotency_key)
        .collect();
    assert_eq!(keys, vec![Some(format!("sms-flow-{id}-1")); 3]);
}

#[test_db::test(seed)]
async fn an_agent_without_a_number_fails_the_run_at_once(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let id = setup.enroll(9700, Some(9600)).await;
    *setup.fakes.text_error.lock().unwrap() = Some("agent_no_number");

    assert_eq!(setup.run_at(noon()).await.failed, 1);
    let after = setup.enrollment(id).await;
    assert_eq!(
        (after.status.as_str(), after.error_message.as_deref()),
        ("failed", Some("agent_no_number"))
    );
}

#[test_db::test(seed)]
async fn a_rep_not_linked_to_the_line_fails_the_run(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let id = setup.enroll(9700, Some(9600)).await;
    sqlx::query("UPDATE users SET cloudtalk_agent_id = NULL WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();

    assert_eq!(setup.run_at(noon()).await.failed, 1);
    assert!(setup.texts().is_empty());
    assert_eq!(
        setup.enrollment(id).await.error_message.as_deref(),
        Some("agent_not_linked")
    );
}

#[test_db::test(seed)]
async fn an_ai_step_is_written_by_the_model_when_it_goes_out(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let id = setup.enroll(9703, Some(9600)).await;
    setup.fakes.replies.lock().unwrap().push_back(json!({
        "skip": false,
        "subject": "",
        "body": "Hi Paula, it's Rita from Acme Stone. Did the quote answer your questions?"
    }));

    let run = setup.run_at(noon()).await;
    assert_eq!(run.sent, 1);
    assert_eq!(
        setup.texts()[0].text,
        "Hi Paula, it's Rita from Acme Stone. Did the quote answer your questions?"
    );
    assert_eq!(setup.enrollment(id).await.status, "completed");
}

#[test_db::test(seed)]
async fn an_ai_step_the_model_skips_completes_the_run(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let id = setup.enroll(9703, Some(9600)).await;
    setup
        .fakes
        .replies
        .lock()
        .unwrap()
        .push_back(json!({ "skip": true, "reason": "nothing new to say" }));

    let run = setup.run_at(noon()).await;
    assert_eq!((run.sent, run.failed), (0, 0));
    assert!(setup.texts().is_empty());
    assert_eq!(setup.enrollment(id).await.status, "completed");
}

#[test_db::test(seed)]
async fn a_paused_or_deleted_flow_sends_nothing(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let paused = setup.enroll(9700, Some(9600)).await;
    sqlx::query("UPDATE sms_flow_enrollments SET status = 'paused' WHERE id = ?")
        .bind(paused)
        .execute(&pool)
        .await
        .unwrap();
    setup.enroll(9701, None).await;
    sqlx::query("UPDATE sms_flows SET deleted_at = '2026-10-02 00:00:00' WHERE id = 9701")
        .execute(&pool)
        .await
        .unwrap();

    assert_eq!(setup.run_at(noon()).await, FollowupRun::default());
    assert!(setup.texts().is_empty());
}

// --- routes ---

async fn post_flow(
    setup: &Setup,
    session: &str,
    path: &str,
    body: Value,
) -> axum_test::TestResponse {
    setup
        .server
        .post(path)
        .with_session(session)
        .json(&body)
        .await
}

#[test_db::test(seed)]
async fn a_rep_starts_pauses_resumes_and_stops_a_customer_flow(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let path = "/v1/customers/9600/flow";

    let res = setup.server.get(path).with_session(sessions::REP).await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>(), json!({ "active_flow": null }));

    let res = post_flow(
        &setup,
        sessions::REP,
        path,
        json!({ "intent": "start", "flow_id": 9704 }),
    )
    .await;
    res.assert_status_ok();
    let flow = &res.json::<Value>()["active_flow"];
    assert_eq!(flow["flow_id"], 9704);
    assert_eq!(flow["flow_name"], "Tomorrow");
    assert_eq!(flow["status"], "active");
    assert_eq!(flow["next_step_preview"], "Tomorrow");
    assert_eq!(flow["total_steps"], 1);
    let row: (Option<i64>, Option<String>, i32) = sqlx::query_as(
        "SELECT customer_phone_digits, customer_email, user_id FROM sms_flow_enrollments
          WHERE customer_id = 9600 AND status = 'active'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        row,
        (Some(3_175_550_177), Some("paula@example.com".into()), 100)
    );

    let res = post_flow(
        &setup,
        sessions::REP,
        path,
        json!({ "intent": "start", "flow_id": 9700 }),
    )
    .await;
    res.assert_status(StatusCode::CONFLICT);
    assert_eq!(res.json::<Value>()["error"], "flow_already_active");
    // Not due yet: the countdown cannot send it.
    let res = post_flow(&setup, sessions::REP, path, json!({ "intent": "dispatch" })).await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(res.json::<Value>()["error"], "not_due");

    let res = post_flow(&setup, sessions::REP, path, json!({ "intent": "pause" })).await;
    assert_eq!(res.json::<Value>()["active_flow"]["status"], "paused");
    let res = post_flow(&setup, sessions::REP, path, json!({ "intent": "pause" })).await;
    assert_eq!(res.json::<Value>()["error"], "invalid_status");
    let res = post_flow(&setup, sessions::REP, path, json!({ "intent": "resume" })).await;
    assert_eq!(res.json::<Value>()["active_flow"]["status"], "active");

    // Switching cancels the running flow in the same step.
    let res = post_flow(
        &setup,
        sessions::REP,
        path,
        json!({ "intent": "switch", "flow_id": 9704 }),
    )
    .await;
    res.assert_status_ok();
    let statuses: Vec<String> = sqlx::query_scalar(
        "SELECT CAST(status AS CHAR) FROM sms_flow_enrollments WHERE customer_id = 9600 ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(statuses, ["cancelled", "active"]);

    let res = post_flow(&setup, sessions::REP, path, json!({ "intent": "stop" })).await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>(), json!({ "active_flow": null }));
    assert!(setup.texts().is_empty());
}

#[test_db::test(seed)]
async fn only_the_starter_the_sales_rep_or_an_admin_changes_a_flow(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let path = "/v1/customers/9600/flow";
    post_flow(
        &setup,
        sessions::REP,
        path,
        json!({ "intent": "start", "flow_id": 9704 }),
    )
    .await
    .assert_status_ok();

    // Mark sees it but cannot change it.
    let res = setup.server.get(path).with_session(sessions::MANAGER).await;
    assert_eq!(res.json::<Value>()["active_flow"]["flow_id"], 9704);
    let res = post_flow(&setup, sessions::MANAGER, path, json!({ "intent": "stop" })).await;
    res.assert_status(StatusCode::FORBIDDEN);
    sqlx::query("UPDATE sms_flows SET user_id = NULL WHERE id = 9700")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET cloudtalk_agent_id = '5002', cloudtalk_phone_number = '3175550001' WHERE id = 101")
        .execute(&pool)
        .await
        .unwrap();
    let res = post_flow(
        &setup,
        sessions::MANAGER,
        path,
        json!({ "intent": "switch", "flow_id": 9700 }),
    )
    .await;
    res.assert_status(StatusCode::FORBIDDEN);

    // Another company's rep finds nothing to change.
    let res = post_flow(
        &setup,
        sessions::OTHER_COMPANY_REP,
        path,
        json!({ "intent": "stop" }),
    )
    .await;
    assert_eq!(res.json::<Value>()["error"], "enrollment_not_found");
    let res = setup
        .server
        .get(path)
        .with_session(sessions::OTHER_COMPANY_REP)
        .await;
    assert_eq!(res.json::<Value>(), json!({ "active_flow": null }));

    let res = post_flow(&setup, sessions::ADMIN, path, json!({ "intent": "stop" })).await;
    res.assert_status_ok();
}

#[test_db::test(seed)]
async fn refused_starts_name_the_reason(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let path = "/v1/customers/9600/flow";
    let cases = [
        (json!({ "intent": "start" }), "validation_failed"),
        (json!({ "intent": "start", "flow_id": 1 }), "flow_not_found"),
        (json!({ "intent": "pause" }), "enrollment_not_found"),
    ];
    for (body, error) in cases {
        let res = post_flow(&setup, sessions::REP, path, body).await;
        res.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(res.json::<Value>()["error"], error);
    }
    // Mark has no CloudTalk agent: no texts in his name.
    let res = post_flow(
        &setup,
        sessions::MANAGER,
        path,
        json!({ "intent": "start", "flow_id": 9704 }),
    )
    .await;
    assert_eq!(res.json::<Value>()["error"], "flow_not_found");
    sqlx::query("UPDATE sms_flows SET user_id = NULL WHERE id = 9704")
        .execute(&pool)
        .await
        .unwrap();
    let res = post_flow(
        &setup,
        sessions::MANAGER,
        path,
        json!({ "intent": "start", "flow_id": 9704 }),
    )
    .await;
    assert_eq!(res.json::<Value>()["error"], "agent_not_linked");
    let res = setup
        .server
        .get("/v1/customers/0/flow")
        .with_session(sessions::REP)
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(res.json::<Value>()["error"], "customer_not_found");
}

#[test_db::test(seed)]
async fn starts_are_limited_to_ten_a_minute(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let path = "/v1/customers/9600/flow";
    for _ in 0..10 {
        let res = post_flow(
            &setup,
            sessions::REP,
            path,
            json!({ "intent": "start", "flow_id": 9704 }),
        )
        .await;
        assert_ne!(res.status_code(), StatusCode::TOO_MANY_REQUESTS);
    }
    let res = post_flow(
        &setup,
        sessions::REP,
        path,
        json!({ "intent": "start", "flow_id": 9704 }),
    )
    .await;
    res.assert_status(StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(res.json::<Value>()["error"], "rate_limited");
    // Other actions are not limited.
    post_flow(&setup, sessions::REP, path, json!({ "intent": "pause" }))
        .await
        .assert_status_ok();
}

#[test_db::test(seed)]
async fn a_thread_flow_runs_on_the_phone(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let path = "/v1/sms/cloudtalk/threads/13175550177/flow";

    let res = post_flow(
        &setup,
        sessions::REP,
        path,
        json!({ "intent": "start", "flow_id": 9704 }),
    )
    .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["active_flow"]["flow_id"], 9704);
    // The phone's customer was linked.
    let customer: Option<i32> = sqlx::query_scalar(
        "SELECT customer_id FROM sms_flow_enrollments WHERE customer_phone_digits = 3175550177 AND status = 'active'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(customer, Some(9600));
    let res = setup.server.get(path).with_session(sessions::REP).await;
    assert_eq!(res.json::<Value>()["active_flow"]["status"], "active");
    // The customer route sees the same run.
    let res = setup
        .server
        .get("/v1/customers/9600/flow")
        .with_session(sessions::REP)
        .await;
    assert_eq!(res.json::<Value>()["active_flow"]["flow_id"], 9704);

    let res = setup
        .server
        .get("/v1/sms/cloudtalk/threads/12ab/flow")
        .with_session(sessions::REP)
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(res.json::<Value>()["error"], "invalid_phone");
    let res = setup.server.get(path).await;
    res.assert_status(StatusCode::UNAUTHORIZED);
}

#[test_db::test(seed)]
async fn send_now_makes_the_next_step_due(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let path = "/v1/customers/9600/flow";
    post_flow(
        &setup,
        sessions::REP,
        path,
        json!({ "intent": "start", "flow_id": 9704 }),
    )
    .await
    .assert_status_ok();

    // Sent right away (or, outside the send window, moved to its opening).
    let res = post_flow(&setup, sessions::REP, path, json!({ "intent": "send_now" })).await;
    res.assert_status_ok();
    let (status, next): (String, Option<chrono::NaiveDateTime>) = sqlx::query_as(
        "SELECT CAST(status AS CHAR), CAST(next_send_at AS DATETIME) FROM sms_flow_enrollments WHERE customer_id = 9600",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let tomorrow = chrono::Utc::now().naive_utc() + chrono::Duration::hours(23);
    if setup.texts().is_empty() {
        assert_eq!(status, "active");
        assert!(next.unwrap() < tomorrow + chrono::Duration::hours(12));
    } else {
        assert_eq!(setup.texts()[0].text, "Tomorrow");
        assert_eq!((status.as_str(), next), ("completed", None));
    }
}

/// Rita texts Paula herself (not a flow, not an automation).
async fn rep_text(pool: &MySqlPool, at: &str) {
    sqlx::query(
        "INSERT INTO cloudtalk_sms (sender, recipient, text, direction, status, agent, sender_user_id, company_id,
                                    created_date, recipient10, phone_digits, is_automated)
         VALUES ('3175550000', '3175550177', 'Sent you the layout', 'outbound', 'sent', '5001', 100, 100,
                 ?, '3175550177', '3175550177', 0)",
    )
    .bind(at)
    .execute(pool)
    .await
    .unwrap();
}

#[test_db::test(seed)]
async fn a_text_from_the_rep_during_the_wait_restarts_it(pool: MySqlPool) {
    let setup = setup(&pool).await;
    // On the second of the two texts, a day after yesterday's send.
    let id = sqlx::query(
        "INSERT INTO sms_flow_enrollments (flow_id, company_id, customer_phone_digits, customer_email, customer_id,
                                           user_id, status, current_step_position, started_at, anchor_at, next_send_at)
         VALUES (9700, 100, '3175550177', 'paula@example.com', 9600, 100, 'active', 2,
                 '2026-10-12 16:00:00', '2026-10-13 16:00:00', '2026-10-01 12:00:00')",
    )
    .execute(&pool)
    .await
    .unwrap()
    .last_insert_id();
    let id = i32::try_from(id).unwrap();
    // Rita texted Paula herself this morning (10 AM in the store).
    rep_text(&pool, "2026-10-14 14:00:00").await;

    let run = setup.run_at(noon()).await;
    assert_eq!((run.processed, run.sent), (1, 0));
    assert!(setup.texts().is_empty());
    let after = setup.enrollment(id).await;
    assert_eq!(
        (after.status.as_str(), after.current_step_position),
        ("active", 2)
    );
    // A day from her text, not from the flow's.
    assert_eq!(after.next_send_at, Some(naive("2026-10-15 14:00:00")));

    let run = setup
        .run_at(parse_iso("2026-10-15T14:05:00Z").unwrap())
        .await;
    assert_eq!(run.sent, 1);
    assert_eq!(setup.texts()[0].text, "Second");
}

#[test_db::test(seed)]
async fn an_approved_ai_follow_up_goes_out_as_approved_when_nothing_changed(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let id = setup.enroll(9705, Some(9600)).await;

    let run = setup.run_at(noon()).await;
    assert_eq!(run.sent, 1);
    assert_eq!(
        setup.texts()[0].text,
        "Want me to resend your quote so it is easy to find?"
    );
    assert!(setup.fakes.prompts.lock().unwrap().is_empty());
    assert_eq!(setup.enrollment(id).await.current_step_position, 2);
}

#[test_db::test(seed)]
async fn an_approved_ai_follow_up_is_written_again_when_the_conversation_changed(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let id = setup.enroll(9705, Some(9600)).await;
    // Rita texted Paula after she approved the follow-up.
    rep_text(&pool, "2026-10-05 15:00:00").await;
    sqlx::query("UPDATE users SET followup_sms_signature = 'Rita\nAcme Stone' WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();
    let rewritten = json!({ "skip": false, "messages": [{
        "skip": false, "subject": "", "body": "Does Thursday or Friday work for a quick call?", "delayMinutes": 0
    }] });
    // A second copy in case the checks ask for a repair.
    setup
        .fakes
        .replies
        .lock()
        .unwrap()
        .extend([rewritten.clone(), rewritten]);

    let run = setup.run_at(noon()).await;
    assert_eq!(run.sent, 1);
    assert_eq!(
        setup.texts()[0].text,
        "Does Thursday or Friday work for a quick call?\n\nRita\nAcme Stone"
    );
    // The preview's writer, told what the approved draft was for.
    let prompts = setup.fakes.prompts.lock().unwrap().clone();
    let system = prompts[0]["messages"][0]["content"].as_str().unwrap();
    assert!(
        system.starts_with("# AI Follow-Up Instructions"),
        "{system}"
    );
    let user = prompts[0]["messages"][1]["content"].as_str().unwrap();
    assert!(user.contains("SEND-TIME REWRITE"), "{user}");
    assert!(
        user.contains("Want me to resend your quote so it is easy to find?"),
        "{user}"
    );
    assert_eq!(setup.enrollment(id).await.current_step_position, 2);
}

#[test_db::test(seed)]
async fn a_changed_offer_rewrites_and_a_skip_completes_the_run(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let id = setup.enroll(9705, Some(9600)).await;
    // The offer changed after Rita approved the follow-up.
    sqlx::query(
        "INSERT INTO ai_followup_offers (company_id, offer, ends_on, updated_by, updated_at)
         VALUES (100, 'Free sink with any kitchen', NULL, 101, '2026-10-06 09:00:00')",
    )
    .execute(&pool)
    .await
    .unwrap();
    setup
        .fakes
        .replies
        .lock()
        .unwrap()
        .push_back(json!({ "skip": true, "reason": "nothing new to say" }));

    let run = setup.run_at(noon()).await;
    assert_eq!(run.sent, 0);
    assert!(setup.texts().is_empty());
    let user = setup.fakes.prompts.lock().unwrap()[0]["messages"][1]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        user.contains("CURRENT OFFER\nFree sink with any kitchen"),
        "{user}"
    );
    assert_eq!(setup.enrollment(id).await.status, "completed");
}

#[test_db::test(seed)]
async fn a_deal_note_after_the_approval_rewrites_the_follow_up(pool: MySqlPool) {
    let setup = setup(&pool).await;
    sqlx::raw_sql(
        "INSERT INTO deals (id, customer_id, title, list_id, position, user_id, created_at, updated_at)
           VALUES (9650, 9600, 'Paula kitchen', 100, 9, 100, '2026-09-01 08:00:00', '2026-09-01 08:00:00');
         INSERT INTO deal_notes (deal_id, company_id, content, created_at, created_by)
           VALUES (9650, 100, 'Old note before the follow-up', '2026-09-20 10:00:00', 'Rita')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let id = setup.enroll(9705, Some(9600)).await;
    // Nothing new since the approval: the approved copy goes out.
    assert_eq!(setup.run_at(noon()).await.sent, 1);
    assert_eq!(
        setup.texts()[0].text,
        "Want me to resend your quote so it is easy to find?"
    );

    sqlx::query(
        "INSERT INTO deal_notes (deal_id, company_id, content, created_at, created_by)
         VALUES (9650, 100, 'Paula picked Calacatta Gold, wants the quote updated', '2026-10-15 10:00:00', 'Rita')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let rewritten = json!({ "skip": false, "messages": [{
        "skip": false, "subject": "", "body": "Want the updated quote by text or email?", "delayMinutes": 0
    }] });
    setup
        .fakes
        .replies
        .lock()
        .unwrap()
        .extend([rewritten.clone(), rewritten]);
    setup.make_due(id).await;
    let run = setup
        .run_at(parse_iso("2026-10-16T16:00:00Z").unwrap())
        .await;
    assert_eq!(run.sent, 1);
    assert_eq!(
        setup.texts()[1].text,
        "Want the updated quote by text or email?"
    );
}

#[test_db::test(seed)]
async fn a_rep_without_a_texting_number_cannot_start_a_text_flow(pool: MySqlPool) {
    let setup = setup(&pool).await;
    sqlx::query("UPDATE users SET cloudtalk_phone_number = NULL WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();
    let path = "/v1/customers/9600/flow";

    let res = post_flow(
        &setup,
        sessions::REP,
        path,
        json!({ "intent": "start", "flow_id": 9704 }),
    )
    .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(res.json::<Value>()["error"], "agent_not_linked");
    let started: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sms_flow_enrollments WHERE customer_id = 9600")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(started, 0);
}

#[test_db::test(seed)]
async fn a_cloudtalk_rep_starts_a_text_flow_where_ringcentral_is_connected_too(pool: MySqlPool) {
    let setup = setup(&pool).await;
    // The company texts through both; Rita has only a CloudTalk line.
    sqlx::query(
        "UPDATE company SET ringcentral_client_id = 'rc-id', ringcentral_client_secret = 'rc-secret',
                            ringcentral_jwt = 'rc-jwt' WHERE id = 100",
    )
    .execute(&pool)
    .await
    .unwrap();

    let res = post_flow(
        &setup,
        sessions::REP,
        "/v1/customers/9600/flow",
        json!({ "intent": "start", "flow_id": 9704 }),
    )
    .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["active_flow"]["status"], "active");
}

/// Paula's open deal with Rita (list 101), from the start of last month.
async fn paula_deal(pool: &MySqlPool) {
    sqlx::query(
        "INSERT INTO deals (id, customer_id, title, list_id, position, user_id, created_at, updated_at)
         VALUES (9650, 9600, 'Paula kitchen', 101, 9, 100, '2026-09-01 08:00:00', '2026-09-01 08:00:00')",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn end_run(pool: &MySqlPool, id: i32, status: &str, error: Option<&str>) {
    sqlx::query(
        "UPDATE sms_flow_enrollments SET status = ?, error_message = ?, next_send_at = NULL,
                updated_at = UTC_TIMESTAMP() WHERE id = ?",
    )
    .bind(status)
    .bind(error)
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
}

async fn notifications(pool: &MySqlPool) -> Vec<(i32, Option<i64>, String, Option<String>)> {
    sqlx::query_as(
        "SELECT user_id, CAST(deal_id AS SIGNED), message, notification_type FROM notifications
          WHERE notification_type LIKE 'followup_%' ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

#[test_db::test(seed)]
async fn a_reply_to_a_follow_up_alerts_the_rep_once(pool: MySqlPool) {
    let setup = setup(&pool).await;
    paula_deal(&pool).await;
    let id = setup.enroll(9700, Some(9600)).await;
    sqlx::query(
        "INSERT INTO cloudtalk_sms (sender, recipient, text, direction, status, company_id, created_date,
                                    phone_digits, sender10, is_echo)
         VALUES ('3175550177', '3175550000', 'Not now,\n maybe in May', 'inbound', 'received', 100,
                 '2026-10-02 09:00:00', '3175550177', '3175550177', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    end_run(&pool, id, "stopped_by_reply", None).await;

    let alerted = api::followups::alerts::alert_ended_followups(&setup.state)
        .await
        .unwrap();
    assert_eq!(alerted, 1);
    assert_eq!(
        notifications(&pool).await,
        vec![(
            100,
            Some(9650),
            "Replied to your follow-up: “Not now, maybe in May”".to_string(),
            Some("followup_replied".to_string())
        )]
    );
    let emails = setup.emails();
    assert_eq!(emails.len(), 1);
    assert_eq!(
        emails[0].subject,
        "Paula Prospect replied to your follow-up"
    );
    assert!(
        emails[0].body.contains("/employee/deals/edit/9650/project"),
        "{}",
        emails[0].body
    );
    // Told once.
    assert_eq!(
        api::followups::alerts::alert_ended_followups(&setup.state)
            .await
            .unwrap(),
        0
    );
    assert_eq!(setup.emails().len(), 1);
}

#[test_db::test(seed)]
async fn a_failed_follow_up_tells_the_rep_why_and_old_ends_stay_quiet(pool: MySqlPool) {
    let setup = setup(&pool).await;
    paula_deal(&pool).await;
    let id = setup.enroll(9700, Some(9600)).await;
    end_run(&pool, id, "failed", Some("agent_not_linked")).await;
    // Ended before alerts existed: counted as told.
    let old = setup.enroll(9701, Some(9600)).await;
    end_run(&pool, old, "stopped_by_reply", None).await;
    sqlx::query("UPDATE sms_flow_enrollments SET ended_alert_at = UTC_TIMESTAMP() WHERE id = ?")
        .bind(old)
        .execute(&pool)
        .await
        .unwrap();

    let alerted = api::followups::alerts::alert_ended_followups(&setup.state)
        .await
        .unwrap();
    assert_eq!(alerted, 1);
    let rows = notifications(&pool).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].3.as_deref(), Some("followup_failed"));
    assert_eq!(
        rows[0].2,
        "Follow-up stopped because your texting line isn't linked to your account. Fix it, then start it again."
    );
    assert_eq!(
        setup.emails()[0].subject,
        "Follow-up for Paula Prospect stopped"
    );
}

#[test_db::test(seed)]
async fn the_9_am_email_lists_quiet_deals_once_a_day(pool: MySqlPool) {
    let setup = setup(&pool).await;
    paula_deal(&pool).await;
    // Rita's last text to Paula was two weeks ago.
    rep_text(&pool, "2026-09-30 10:00:00").await;
    let rita: String = sqlx::query_scalar("SELECT TRIM(email) FROM users WHERE id = 100")
        .fetch_one(&pool)
        .await
        .unwrap();
    // Wednesday 9:30 AM in the store.
    let nine = parse_iso("2026-10-14T13:30:00Z").unwrap();

    let sent = api::followups::digest::send_quiet_digests(&setup.state, nine)
        .await
        .unwrap();
    assert!(sent >= 1);
    let emails = setup.emails();
    let to_rita: Vec<_> = emails.iter().filter(|e| e.to == [rita.clone()]).collect();
    assert_eq!(to_rita.len(), 1);
    assert!(
        to_rita[0].subject.ends_with("waiting on you"),
        "{}",
        to_rita[0].subject
    );
    assert!(
        to_rita[0].body.contains(
            "/employee/deals/edit/9650/project\">Paula Prospect</a> · Quoted · last message 14 days ago"
        ),
        "{}",
        to_rita[0].body
    );

    // Once a day, at 9 only, never on Sunday.
    let count = setup.emails().len();
    assert_eq!(
        api::followups::digest::send_quiet_digests(&setup.state, nine)
            .await
            .unwrap(),
        0
    );
    for at in ["2026-10-14T14:30:00Z", "2026-10-18T13:30:00Z"] {
        assert_eq!(
            api::followups::digest::send_quiet_digests(&setup.state, parse_iso(at).unwrap())
                .await
                .unwrap(),
            0
        );
    }
    assert_eq!(setup.emails().len(), count);
}

#[test_db::test(seed)]
async fn a_customer_with_a_follow_up_running_is_not_listed(pool: MySqlPool) {
    let setup = setup(&pool).await;
    paula_deal(&pool).await;
    rep_text(&pool, "2026-09-30 10:00:00").await;
    setup.enroll(9704, Some(9600)).await;
    let deals = api::followups::digest::quiet_deals(
        &setup.state,
        100,
        100,
        parse_iso("2026-10-14T13:30:00Z").unwrap(),
    )
    .await
    .unwrap();
    assert!(deals.iter().all(|d| d.deal_id != 9650), "{deals:?}");
}
