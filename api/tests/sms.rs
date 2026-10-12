mod common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::Value;
use sqlx::MySqlPool;

// Company 100 texting through CloudTalk. Rita (agent 5001, own number
// 317-555-0900) and Mark (agent 5002); 317-555-0999 is a company line.
//
// - A, Alice 3175550100: Rita wrote (9000), Alice answered (9001, unread);
//   9006 is a hidden echo of 9000.
// - B, Bob 3175550102: Mark's conversation; Bob's "Need a quote" is unread.
// - C, 3175550777: a stranger texted Rita's own number (unread).
// - D, the supplier 3175550888: Rita sent a photo (9005); 9007 is a send
//   stuck pending since 2025, failed by the upkeep.
const FIXTURE: &str = "
UPDATE users SET cloudtalk_agent_id = '5001', cloudtalk_phone_number = '+1 (317) 555-0900' WHERE id = 100;
UPDATE users SET cloudtalk_agent_id = '5002' WHERE id = 101;
INSERT INTO sms_company_phones (provider, company_id, phone10, agent_sender, inbound_recipient)
  VALUES ('cloudtalk', 100, '3175550999', 1, 1);
INSERT INTO cloudtalk_contacts (customer_id, company_id, cloudtalk_id, phone_e164_1)
  VALUES (1000, 100, 77, '+13175550100');
INSERT INTO suppliers (id, supplier_name, phone, company_id) VALUES (9100, 'Granite Supply', '(317) 555-0888', 100);
INSERT INTO cloudtalk_sms (id, cloudtalk_id, sender, recipient, text, direction, status, agent, sender_user_id,
                           company_id, created_date, sender10, recipient10, phone_digits, is_echo, echo_of_sms_id) VALUES
  (9000, 100, 3175550999, 3175550100, 'Hello Alice', 'outbound', 'sent', '5001', 100, 100, '2025-06-01 10:00:00', '3175550999', '3175550100', '3175550100', 0, NULL),
  (9001, 101, 3175550100, 3175550999, 'Thanks!', 'inbound', 'received', NULL, NULL, 100, '2025-06-01 10:05:00', '3175550100', '3175550999', '3175550100', 0, NULL),
  (9006, 102, 3175550999, 3175550100, 'Hello Alice', 'inbound', 'received', NULL, NULL, 100, '2025-06-01 10:00:01', '3175550999', '3175550100', '3175550100', 1, 9000),
  (9002, 103, 3175550999, 3175550102, 'Hi Bob', 'outbound', 'sent', '5002', 101, 100, '2025-06-02 10:00:00', '3175550999', '3175550102', '3175550102', 0, NULL),
  (9003, 104, 3175550102, 3175550999, 'Need a quote', 'inbound', 'received', NULL, NULL, 100, '2025-06-02 10:10:00', '3175550102', '3175550999', '3175550102', 0, NULL),
  (9004, 105, 3175550777, 3175550900, 'Is this Rita?', 'inbound', 'received', NULL, NULL, 100, '2025-06-03 09:00:00', '3175550777', '3175550900', '3175550777', 0, NULL),
  (9005, 106, 3175550999, 3175550888, '', 'outbound', 'sent', '5001', 100, 100, '2025-06-04 10:00:00', '3175550999', '3175550888', '3175550888', 0, NULL),
  (9007, NULL, NULL, 3175550888, 'Stuck', 'outbound', 'pending', '5001', 100, 100, '2025-06-04 09:00:00', NULL, '3175550888', '3175550888', 0, NULL);
INSERT INTO cloudtalk_sms_attachments (cloudtalk_sms_id, content_type, filename, s3_key, s3_url, position)
  VALUES (9005, 'image/jpeg', 'slab.jpg', 'sms-attachments/100/x/slab.jpg', 'https://bucket/slab.jpg', 0);
";

async fn seed(pool: &MySqlPool) {
    sqlx::raw_sql(FIXTURE).execute(pool).await.unwrap();
}

async fn get(pool: &MySqlPool, path: &str, session: &str) -> Value {
    let res = server(pool.clone()).get(path).with_session(session).await;
    res.assert_status_ok();
    res.json()
}

fn phones(list: &Value) -> Vec<&str> {
    list["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["phone_digits"].as_str().unwrap())
        .collect()
}

const THREADS: &str = "/v1/sms/cloudtalk/threads";

#[test_db::test(seed)]
async fn a_rep_sees_own_and_own_number_threads_unread_first(pool: MySqlPool) {
    seed(&pool).await;
    let list = get(&pool, THREADS, sessions::REP).await;
    assert_eq!(phones(&list), ["3175550777", "3175550100", "3175550888"]);
    assert_eq!(list["total_count"], 3);
    assert_eq!(list["unread_count"], 2);
    assert_eq!(list["has_more"], false);

    let alice = &list["threads"][1];
    assert_eq!(alice["customer_id"], 1000);
    assert_eq!(alice["customer_name"], "Alice Lead");
    assert_eq!(alice["last_message_text"], "Thanks!");
    assert_eq!(alice["last_direction"], "inbound");
    assert_eq!(alice["message_count"], 2);
    assert_eq!(alice["unread_count"], 1);

    let supplier = &list["threads"][2];
    assert_eq!(supplier["customer_name"], "Granite Supply");
    assert_eq!(supplier["last_message_text"], "📷 Photo");
    assert_eq!(supplier["last_direction"], "outbound");
    assert_eq!(supplier["unread_count"], 0);

    let page = get(&pool, &format!("{THREADS}?limit=1&offset=1"), sessions::REP).await;
    assert_eq!(phones(&page), ["3175550100"]);
    assert_eq!(page["has_more"], true);
}

#[test_db::test(seed)]
async fn admins_see_the_company_and_filter_by_rep(pool: MySqlPool) {
    seed(&pool).await;
    let all = get(&pool, &format!("{THREADS}?scope=all"), sessions::ADMIN).await;
    assert_eq!(
        phones(&all),
        ["3175550888", "3175550777", "3175550102", "3175550100"]
    );
    assert_eq!(all["unread_count"], 3);

    let mark = get(
        &pool,
        &format!("{THREADS}?scope=all&agent_id=5002"),
        sessions::ADMIN,
    )
    .await;
    assert_eq!(phones(&mark), ["3175550102"]);

    // A rep asking for everything gets their own.
    let rep = get(&pool, &format!("{THREADS}?scope=all"), sessions::REP).await;
    assert_eq!(rep["total_count"], 3);
}

#[test_db::test(seed)]
async fn search_matches_text_digits_and_names(pool: MySqlPool) {
    seed(&pool).await;
    let all = |term: &str| format!("{THREADS}?scope=all&search={term}");
    assert_eq!(
        phones(&get(&pool, &all("quote"), sessions::ADMIN).await),
        ["3175550102"]
    );
    assert_eq!(
        phones(&get(&pool, &all("0777"), sessions::ADMIN).await),
        ["3175550777"]
    );
    assert_eq!(
        phones(&get(&pool, &all("Alic"), sessions::ADMIN).await),
        ["3175550100"]
    );
    assert_eq!(
        phones(&get(&pool, &all("Granite"), sessions::ADMIN).await),
        ["3175550888"]
    );
    let none = get(&pool, &all("zzzz"), sessions::ADMIN).await;
    assert_eq!(none["total_count"], 0);
    // The badge still counts everything unread.
    assert_eq!(none["unread_count"], 3);
}

#[test_db::test(seed)]
async fn a_thread_lists_visible_messages_oldest_first(pool: MySqlPool) {
    seed(&pool).await;
    sqlx::raw_sql(
        "INSERT INTO sms_flows (id, company_id, name, created_at) VALUES (9200, 100, 'Nurture', NOW());
         INSERT INTO sms_flow_steps (flow_id, position, custom_text, delay_minutes) VALUES (9200, 1, ' Still there? ', 0);
         INSERT INTO sms_flow_enrollments (flow_id, company_id, customer_phone_digits, user_id, status, anchor_at)
           VALUES (9200, 100, 3175550100, 101, 'active', NOW());",
    )
    .execute(&pool)
    .await
    .unwrap();
    let thread = get(&pool, &format!("{THREADS}/3175550100"), sessions::REP).await;
    let texts: Vec<&str> = thread["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts, ["Hello Alice", "Thanks!"]);
    assert_eq!(thread["messages"][0]["direction"], "outbound");
    assert_eq!(thread["messages"][0]["agent_id"], "5001");
    assert_eq!(thread["messages"][0]["agent_name"], "Rita Rep");
    assert_eq!(thread["messages"][0]["status"], "sent");
    assert_eq!(thread["messages"][1]["direction"], "inbound");
    assert_eq!(thread["customer"]["name"], "Alice Lead");
    assert_eq!(thread["customer"]["sales_rep_name"], "Rita Rep");
    assert_eq!(thread["can_send"], true);
    assert_eq!(thread["has_older"], false);
    assert_eq!(thread["unread_count"], 0);
    assert_eq!(thread["active_flow"]["flow_name"], "Nurture");
    assert_eq!(thread["active_flow"]["next_step_preview"], "Still there?");
    assert_eq!(thread["active_flow"]["total_steps"], 1);

    let older = get(
        &pool,
        &format!("{THREADS}/3175550100?limit=1&before_id=9001"),
        sessions::REP,
    )
    .await;
    assert_eq!(older["messages"][0]["id"], 9000);
    assert_eq!(older["has_older"], false);
    let newest = get(
        &pool,
        &format!("{THREADS}/3175550100?limit=1"),
        sessions::REP,
    )
    .await;
    assert_eq!(newest["messages"][0]["id"], 9001);
    assert_eq!(newest["has_older"], true);

    let admin = get(
        &pool,
        &format!("{THREADS}/3175550100?scope=all"),
        sessions::ADMIN,
    )
    .await;
    assert_eq!(admin["unread_count"], 1);
    assert_eq!(admin["can_send"], false);
}

#[test_db::test(seed)]
async fn hidden_threads_stay_hidden(pool: MySqlPool) {
    seed(&pool).await;
    // Mark's conversation, to Rita and to another company.
    for session in [sessions::REP, sessions::OTHER_COMPANY_REP] {
        let thread = get(&pool, &format!("{THREADS}/3175550102"), session).await;
        assert_eq!(thread["messages"], serde_json::json!([]));
    }
    let other = get(&pool, THREADS, sessions::OTHER_COMPANY_REP).await;
    assert_eq!(other["total_count"], 0);
    let server = server(pool.clone());
    server
        .get(&format!("{THREADS}/5550101999"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .get(&format!("{THREADS}/12-34"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .get("/v1/sms/telnyx/threads")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

async fn unread(pool: &MySqlPool) -> Value {
    get(pool, "/v1/sms/cloudtalk/unread-count", sessions::REP).await["count"].clone()
}

async fn post(pool: &MySqlPool, path: &str) {
    server(pool.clone())
        .post(path)
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NO_CONTENT);
}

#[test_db::test(seed)]
async fn read_markers_drive_the_badge(pool: MySqlPool) {
    seed(&pool).await;
    assert_eq!(unread(&pool).await, 2);
    post(&pool, &format!("{THREADS}/3175550100/read")).await;
    assert_eq!(unread(&pool).await, 1);
    post(&pool, &format!("{THREADS}/3175550888/unread")).await;
    assert_eq!(unread(&pool).await, 2);
    let list = get(&pool, THREADS, sessions::REP).await;
    assert_eq!(list["threads"][2]["unread_count"], 0);
    assert_eq!(list["threads"][0]["phone_digits"], "3175550888");
    assert_eq!(list["threads"][0]["unread_count"], 1);
    // Mark's thread is not Rita's to mark.
    post(&pool, &format!("{THREADS}/3175550102/read")).await;
    let marked: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM cloudtalk_sms_thread_reads WHERE customer_phone_digits = '3175550102'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(marked, 0);
}

#[test_db::test(seed)]
async fn desktop_alerts_need_the_provider(pool: MySqlPool) {
    seed(&pool).await;
    let alerts = get(&pool, "/v1/sms/cloudtalk/desktop-alerts", sessions::REP).await;
    assert_eq!(alerts["alerts"], serde_json::json!([]));
    sqlx::query("UPDATE company SET cloudtalk_access_key = 'k', cloudtalk_access_secret = 's' WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();
    let alerts = get(&pool, "/v1/sms/cloudtalk/desktop-alerts", sessions::REP).await;
    let alerts = alerts["alerts"].as_array().unwrap();
    assert_eq!(alerts.len(), 2);
    assert_eq!(alerts[0]["id"], "sms-3175550777");
    assert_eq!(alerts[0]["title"], "3175550777");
    assert_eq!(alerts[0]["message"], "Is this Rita?");
    assert_eq!(alerts[0]["href"], "/employee/cloudtalk/thread/3175550777");
    assert_eq!(alerts[1]["title"], "Alice Lead");
}

#[test_db::test(seed)]
async fn stuck_sends_fail_on_read(pool: MySqlPool) {
    seed(&pool).await;
    // The supplier's echo of the stuck send, hidden behind it.
    sqlx::raw_sql(
        "INSERT INTO cloudtalk_sms (id, cloudtalk_id, sender, recipient, text, direction, status, company_id,
                                    created_date, sender10, recipient10, phone_digits, is_echo, echo_of_sms_id)
         VALUES (9008, 107, 3175550999, 3175550888, 'Stuck', 'inbound', 'received', 100,
                 '2025-06-04 09:00:02', '3175550999', '3175550888', '3175550888', 1, 9007)",
    )
    .execute(&pool)
    .await
    .unwrap();
    get(&pool, THREADS, sessions::REP).await;
    let (status, error): (String, Option<String>) = sqlx::query_as(
        "SELECT CAST(status AS CHAR), error_message FROM cloudtalk_sms WHERE id = 9007",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "failed");
    assert_eq!(error.as_deref(), Some("pending_timeout"));
    let echo: (i8, Option<i32>) =
        sqlx::query_as("SELECT is_echo, echo_of_sms_id FROM cloudtalk_sms WHERE id = 9008")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(echo, (0, None));
}

#[test_db::test(seed)]
async fn admins_list_reps_who_text(pool: MySqlPool) {
    seed(&pool).await;
    let reps = get(&pool, "/v1/sms/cloudtalk/sales-reps", sessions::ADMIN).await;
    assert_eq!(
        reps,
        serde_json::json!([
            { "agent_id": "5002", "name": "Mark Manager" },
            { "agent_id": "5001", "name": "Rita Rep" },
        ])
    );
    server(pool)
        .get("/v1/sms/cloudtalk/sales-reps")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn a_late_cloudtalk_webhook_sits_between_its_neighbours(pool: MySqlPool) {
    seed(&pool).await;
    // 9302 (provider id 302) arrived at 10:30 but was sent between 301 and 303.
    sqlx::raw_sql(
        "INSERT INTO cloudtalk_sms (id, cloudtalk_id, sender, recipient, text, direction, status, agent,
                                    company_id, created_date, sender10, recipient10, phone_digits) VALUES
           (9301, 301, 3175550999, 3175550555, 'first',  'outbound', 'sent', '5001', 100, '2025-07-01 10:00:00', '3175550999', '3175550555', '3175550555'),
           (9302, 302, 3175550999, 3175550555, 'late',   'outbound', 'sent', '5001', 100, '2025-07-01 10:30:00', '3175550999', '3175550555', '3175550555'),
           (9303, 303, 3175550999, 3175550555, 'third',  'outbound', 'sent', '5001', 100, '2025-07-01 10:10:00', '3175550999', '3175550555', '3175550555');",
    )
    .execute(&pool)
    .await
    .unwrap();
    let thread = get(&pool, &format!("{THREADS}/3175550555"), sessions::REP).await;
    let texts: Vec<&str> = thread["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts, ["first", "late", "third"]);
    assert_eq!(thread["messages"][1]["created_at"], "2025-07-01T10:05:00Z");
}

#[test_db::test(seed)]
async fn search_finds_names_in_any_word_order(pool: MySqlPool) {
    seed(&pool).await;
    let found = get(
        &pool,
        &format!("{THREADS}?search=Lead%20Alic"),
        sessions::REP,
    )
    .await;
    assert_eq!(phones(&found), ["3175550100"]);
}
