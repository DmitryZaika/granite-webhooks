mod common;

use axum::http::StatusCode;
use common::{WithSession, ids, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Seed: "Sales questionnaires" block (8720-8735).

async fn get(pool: &MySqlPool, path: &str, session: &str) -> Value {
    let res = server(pool.clone()).get(path).with_session(session).await;
    res.assert_status_ok();
    res.json()
}

#[test_db::test(seed)]
async fn deal_questionnaires_are_shared_plus_own_with_answers(pool: MySqlPool) {
    assert_eq!(
        get(&pool, "/v1/deals/8600/questionnaires", sessions::DEALER).await,
        json!([
            { "id": 8720, "name": "Remodel Discovery", "questions": [
                { "id": 8730, "question": "What type of remodel?", "answer": "Kitchen" },
                { "id": 8731, "question": "Budget?", "answer": "" }
            ]},
            { "id": 8721, "name": "Dina checklist", "questions": [
                { "id": 8733, "question": "Sink style?", "answer": "" }
            ]}
        ])
    );
    // Rita sees her own instead of Dina's.
    let rita = get(&pool, "/v1/deals/8600/questionnaires", sessions::REP).await;
    assert_eq!(ids(&rita), vec![8720, 8722]);
    server(pool)
        .get("/v1/deals/8009/questionnaires")
        .with_session(sessions::DEALER)
        .await
        .assert_status_not_found();
}

#[test_db::test(seed)]
async fn answers_are_saved_and_can_become_a_note(pool: MySqlPool) {
    let app = server(pool.clone());
    let res = app
        .put("/v1/deals/8601/questionnaire-answers")
        .with_session(sessions::DEALER)
        .json(&json!({ "answers": [
            { "question_id": 8730, "answer": "Bathroom" },
            { "question_id": 8733, "answer": "Undermount" },
            { "question_id": 8734, "answer": "Not mine" }
        ]}))
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>(), json!({ "note_created": false }));
    let list = get(&pool, "/v1/deals/8601/questionnaires", sessions::DEALER).await;
    assert_eq!(list[0]["questions"][0]["answer"], "Bathroom");
    assert_eq!(list[1]["questions"][0]["answer"], "Undermount");
    let foreign: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sales_questionnaire_responses WHERE question_id = 8734",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(foreign, 0, "Rita's question is ignored for Dina");

    // Again, as a note: answers overwrite, the note lists the answered ones.
    let res = app
        .put("/v1/deals/8601/questionnaire-answers")
        .with_session(sessions::DEALER)
        .json(&json!({ "create_note": true, "answers": [
            { "question_id": 8730, "answer": " Kitchen " },
            { "question_id": 8731, "answer": "" },
            { "question_id": 8733, "answer": "Farmhouse" }
        ]}))
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>(), json!({ "note_created": true }));
    let note: String = sqlx::query_scalar(
        "SELECT content FROM deal_notes WHERE deal_id = 8601 ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        note,
        "Questionnaire: Remodel Discovery\n\n1. What type of remodel?\n- **Kitchen**\n\nQuestionnaire: Dina checklist\n\n1. Sink style?\n- **Farmhouse**"
    );
}

#[test_db::test(seed)]
async fn answer_validation(pool: MySqlPool) {
    let app = server(pool);
    app.put("/v1/deals/8601/questionnaire-answers")
        .with_session(sessions::DEALER)
        .json(&json!({ "create_note": true, "answers": [{ "question_id": 8731, "answer": "  " }] }))
        .await
        .assert_status_bad_request();
    app.put("/v1/deals/8601/questionnaire-answers")
        .with_session(sessions::DEALER)
        .json(&json!({ "answers": [{ "question_id": 8731, "answer": "x".repeat(10_001) }] }))
        .await
        .assert_status_bad_request();
    app.put("/v1/deals/8601/questionnaire-answers")
        .with_session(sessions::DEALER)
        .json(&json!({ "create_note": true, "answers": [{ "question_id": 8731, "answer": "x".repeat(5001) }] }))
        .await
        .assert_status_bad_request();
    app.put("/v1/deals/8009/questionnaire-answers")
        .with_session(sessions::DEALER)
        .json(&json!({ "answers": [] }))
        .await
        .assert_status_not_found();
}

#[test_db::test(seed)]
async fn manage_own_questionnaires(pool: MySqlPool) {
    let app = server(pool.clone());
    assert_eq!(
        get(&pool, "/v1/questionnaires", sessions::DEALER).await,
        json!([{ "id": 8721, "name": "Dina checklist", "created_at": "2026-04-02T10:00:00.000Z",
                 "questions": [{ "id": 8733, "question": "Sink style?" }] }])
    );
    let res = app
        .post("/v1/questionnaires")
        .with_session(sessions::DEALER)
        .json(&json!({ "name": " Install ", "questions": ["Cabinets level?", "Water off?"] }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let id = res.json::<Value>()["id"].as_i64().unwrap();
    let list = get(&pool, "/v1/questionnaires", sessions::DEALER).await;
    assert_eq!(list[0]["name"], "Install");
    assert_eq!(list[0]["questions"].as_array().unwrap().len(), 2);

    app.patch(&format!("/v1/questionnaires/{id}"))
        .with_session(sessions::DEALER)
        .json(&json!({ "name": "Install day" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let res = app
        .post(&format!("/v1/questionnaires/{id}/questions"))
        .with_session(sessions::DEALER)
        .json(&json!({ "question": "Photos taken?" }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let question = res.json::<Value>()["id"].as_i64().unwrap();
    app.patch(&format!("/v1/questionnaires/{id}/questions/{question}"))
        .with_session(sessions::DEALER)
        .json(&json!({ "question": "Photos uploaded?" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    app.delete(&format!("/v1/questionnaires/{id}/questions/{question}"))
        .with_session(sessions::DEALER)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let list = get(&pool, "/v1/questionnaires", sessions::DEALER).await;
    assert_eq!(list[0]["name"], "Install day");
    assert_eq!(list[0]["questions"].as_array().unwrap().len(), 2);

    // Default questionnaire, then deleting it unsets the default.
    app.put("/v1/me/default-questionnaire")
        .with_session(sessions::DEALER)
        .json(&json!({ "questionnaire_id": id }))
        .await
        .assert_status_ok();
    assert_eq!(
        get(&pool, "/v1/me/default-questionnaire", sessions::DEALER).await,
        json!({ "questionnaire_id": id })
    );
    app.delete(&format!("/v1/questionnaires/{id}"))
        .with_session(sessions::DEALER)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        get(&pool, "/v1/me/default-questionnaire", sessions::DEALER).await,
        json!({ "questionnaire_id": null })
    );
}

#[test_db::test(seed)]
async fn only_own_questionnaires_are_editable(pool: MySqlPool) {
    let app = server(pool);
    for id in [8720, 8722, 8723, 8724] {
        app.patch(&format!("/v1/questionnaires/{id}"))
            .with_session(sessions::DEALER)
            .json(&json!({ "name": "x" }))
            .await
            .assert_status_not_found();
    }
    // A question of another questionnaire, a deleted question.
    app.patch("/v1/questionnaires/8721/questions/8730")
        .with_session(sessions::DEALER)
        .json(&json!({ "question": "x" }))
        .await
        .assert_status_not_found();
    for body in [
        json!({ "name": "", "questions": ["q"] }),
        json!({ "name": "n", "questions": [] }),
        json!({ "name": "n", "questions": [" "] }),
    ] {
        app.post("/v1/questionnaires")
            .with_session(sessions::DEALER)
            .json(&body)
            .await
            .assert_status_bad_request();
    }
    // Shared questionnaires can be the default; other companies' cannot.
    app.put("/v1/me/default-questionnaire")
        .with_session(sessions::DEALER)
        .json(&json!({ "questionnaire_id": 8720 }))
        .await
        .assert_status_ok();
    app.put("/v1/me/default-questionnaire")
        .with_session(sessions::DEALER)
        .json(&json!({ "questionnaire_id": 8723 }))
        .await
        .assert_status_not_found();
}
