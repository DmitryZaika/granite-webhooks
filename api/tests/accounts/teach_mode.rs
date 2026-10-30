use crate::common;

use api::ai::Ai;
use api::app;
use api::state::AppState;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

/// Instruction 9200 of company 100 with a published question (9201, answer
/// 9212 right) and a draft (9202); instruction 9300 belongs to company 101.
async fn seed(pool: &MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO instructions (id, title, rich_text, company_id)
           VALUES (9200, 'Sealing', '<p>Seal every <b>edge</b>.</p>', 100),
                  (9300, 'Other', '<p>x</p>', 101);
         INSERT INTO questions (id, text, instruction_id, question_type, company_id, is_visible_to_employees, created_date)
           VALUES (9201, 'What to seal?', 9200, 'MC', 100, 1, '2026-01-01 10:00:00'),
                  (9202, 'Draft question', 9200, 'MC', 100, 0, '2026-01-02 10:00:00');
         INSERT INTO answer_choices (id, question_id, text, is_correct)
           VALUES (9211, 9201, 'Nothing', 0), (9212, 9201, 'Every edge', 1),
                  (9221, 9202, 'A', 1), (9222, 9202, 'B', 0);",
    )
    .execute(pool)
    .await
    .unwrap();
}

#[test_db::test(seed)]
async fn an_employee_takes_the_test_and_the_server_grades_it(pool: MySqlPool) {
    seed(&pool).await;
    let server = server(pool.clone());
    let test = server
        .get("/v1/teach-mode")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(common::ids(&test["questions"]), [9201]);
    assert_eq!(common::ids(&test["answer_choices"]), [9211, 9212]);
    assert_eq!(
        test["instructions"],
        json!([{ "id": 9200, "title": "Sealing", "completed": false }])
    );

    let wrong = server
        .post("/v1/teach-mode/attempts")
        .with_session(sessions::REP)
        .json(&json!({ "answers": [{ "question_id": 9201, "selected_answer_id": 9211 }] }))
        .await
        .json::<Value>();
    assert_eq!(wrong, json!({ "score_percent": 0, "passed": false }));
    // The page cannot claim a right answer: the server checks the choice.
    let right = server
        .post("/v1/teach-mode/attempts")
        .with_session(sessions::REP)
        .json(&json!({ "answers": [{ "question_id": 9201, "selected_answer_id": 9212 }] }))
        .await
        .json::<Value>();
    assert_eq!(right, json!({ "score_percent": 100, "passed": true }));
    let tries: Vec<(i32, i32, bool)> = sqlx::query_as(
        "SELECT employee_id, attempt_number, is_correct = 1 FROM answer_attempts
          WHERE question_id = 9201 ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(tries, [(100, 1, false), (100, 2, true)]);
    let test = server
        .get("/v1/teach-mode")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(test["instructions"][0]["completed"], true);

    // Drafts and other companies' questions cannot be answered.
    for question in [9202, 1] {
        server
            .post("/v1/teach-mode/attempts")
            .with_session(sessions::REP)
            .json(&json!({ "answers": [{ "question_id": question, "selected_answer_id": null }] }))
            .await
            .assert_status_bad_request();
    }
}

#[test_db::test(seed)]
async fn an_admin_manages_questions(pool: MySqlPool) {
    seed(&pool).await;
    let server = server(pool.clone());
    server
        .get("/v1/teach-mode/questions")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let all = server
        .get("/v1/teach-mode/questions")
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>();
    assert_eq!(common::ids(&all["questions"]), [9202, 9201]);
    assert_eq!(all["questions"][0]["is_visible_to_employees"], false);

    let bad = server
        .post("/v1/teach-mode/questions")
        .with_session(sessions::ADMIN)
        .json(&json!({ "instruction_id": 9200, "text": "Q", "choices": [
            { "text": "A", "is_correct": true }, { "text": "B", "is_correct": true } ] }))
        .await;
    bad.assert_status_bad_request();
    assert_eq!(
        bad.json::<Value>()["error"],
        "Choose exactly one correct answer"
    );
    server
        .post("/v1/teach-mode/questions")
        .with_session(sessions::ADMIN)
        .json(&json!({ "instruction_id": 9300, "text": "Q", "choices": [
            { "text": "A", "is_correct": true }, { "text": "B", "is_correct": false } ] }))
        .await
        .assert_status_bad_request();
    let created = server
        .post("/v1/teach-mode/questions")
        .with_session(sessions::ADMIN)
        .json(
            &json!({ "instruction_id": 9200, "text": " How long? ", "choices": [
            { "text": "1h", "is_correct": false }, { "text": "24h", "is_correct": true },
            { "text": "  ", "is_correct": false } ] }),
        )
        .await;
    created.assert_status(StatusCode::CREATED);
    let id = created.json::<Value>()["id"].as_i64().unwrap();
    let saved: Vec<(String, bool)> = sqlx::query_as(
        "SELECT text, is_correct = 1 FROM answer_choices WHERE question_id = ? ORDER BY id",
    )
    .bind(id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(saved, [("1h".into(), false), ("24h".into(), true)]);

    // Edit: keep 9212 (now wrong), drop 9211, add a right one.
    server
        .put("/v1/teach-mode/questions/9201")
        .with_session(sessions::ADMIN)
        .json(&json!({ "text": "Seal what?", "choices": [
            { "id": 9212, "text": "Every edge", "is_correct": false },
            { "text": "Only the top", "is_correct": true } ] }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let after = server
        .get("/v1/teach-mode/questions")
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>();
    let choices: Vec<(String, bool)> = after["answer_choices"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["question_id"] == 9201)
        .map(|c| {
            (
                c["text"].as_str().unwrap().to_string(),
                c["is_correct"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        choices,
        [("Every edge".into(), false), ("Only the top".into(), true)]
    );

    server
        .put("/v1/teach-mode/questions/9201/visibility")
        .with_session(sessions::ADMIN)
        .json(&json!({ "visible": false }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let published = server
        .post("/v1/teach-mode/instructions/9200/publish")
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>();
    assert_eq!(published, json!({ "count": 2 }));
    server
        .delete("/v1/teach-mode/questions/9202")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .delete("/v1/teach-mode/questions/9202")
        .with_session(sessions::ADMIN)
        .await
        .assert_status_not_found();
}

#[test_db::test(seed)]
async fn generated_questions_are_added_published(pool: MySqlPool) {
    seed(&pool).await;
    let router = Router::new().route(
        "/chat",
        post(|Json(body): Json<Value>| async move {
            assert_eq!(body["response_format"]["type"], "json_object");
            let prompt = body["messages"][1]["content"].as_str().unwrap();
            assert!(prompt.contains("Seal every edge ."));
            assert!(prompt.contains("- What to seal?"));
            let reply = json!({ "questions": [
                { "question": "Which edges?", "options": ["All", "None", "Top", "Back"], "correctOptionIndex": 0 },
                { "question": "Broken", "options": ["a"], "correctOptionIndex": 0 },
            ] });
            Json(json!({
                "choices": [{ "message": { "content": reply.to_string() } }],
                "usage": { "prompt_tokens": 10, "completion_tokens": 10 },
            }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_ai(Ai::new(Some("key"), &format!("http://{address}/chat")));
    let server = TestServer::new(app(state));
    let generated = server
        .post("/v1/teach-mode/instructions/9200/generate")
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>();
    assert_eq!(generated, json!({ "count": 1 }));
    let added: (String, bool, String) = sqlx::query_as(
        "SELECT q.text, q.is_visible_to_employees = 1, ac.text FROM questions q
           JOIN answer_choices ac ON ac.question_id = q.id AND ac.is_correct = 1
          WHERE q.text = 'Which edges?'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(added, ("Which edges?".into(), true, "All".into()));
}
