//! The CRM manual chat's eval (`fixtures/crm_manual_eval.json`): realistic
//! questions, typos included, and the manual sections that hold each answer.
//! Run with `--nocapture` for the report. `--ignored` also asks the real
//! model (GitHub Actions has `OPEN_AI_SECRET_KEY`):
//!
//! `gh workflow run "API tests" --ref <branch> -f cargo_args="-p api --test crm_manual_eval -- --ignored --nocapture"`

mod common;

use api::ai::{Ai, Caller};
use api::app;
use api::chat::crm_manual::{self, Features};
use api::chat::{Mode, mode_for};
use api::state::AppState;
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use futures_util::future::join_all;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::MySqlPool;

#[derive(Deserialize)]
struct Case {
    question: String,
    /// The sections that hold the answer (any one of them).
    sections: Vec<String>,
    /// About using the app (the manual), not the company's process or records.
    #[serde(default = "yes")]
    app: bool,
    #[serde(default)]
    assistant: bool,
    /// Several features fit: ask which.
    #[serde(default)]
    clarify: bool,
    #[serde(default)]
    role: Option<String>,
    /// One of the questions users reported.
    #[serde(default)]
    example: bool,
}

const fn yes() -> bool {
    true
}

fn cases() -> Vec<Case> {
    serde_json::from_str(include_str!("fixtures/crm_manual_eval.json")).expect("valid eval")
}

#[allow(clippy::cast_precision_loss)]
fn percent(part: usize, whole: usize) -> f64 {
    100.0 * part as f64 / whole.max(1) as f64
}

/// The manual text the sections add to the core ones.
fn size(ids: &[String]) -> usize {
    ids.iter()
        .filter_map(|id| crm_manual::section(id))
        .map(str::len)
        .sum()
}

#[test]
fn retrieval_and_routing_report() {
    let cases = cases();
    let about_the_app: Vec<&Case> = cases.iter().filter(|c| c.app).collect();
    let mut hits = 0;
    let mut how_to = 0;
    let mut to_assistant = 0;
    let mut extra_sections = 0;
    let mut chars = 0;
    let mut misses = Vec::new();
    let mut not_how_to = Vec::new();
    for case in &about_the_app {
        let ids = crm_manual::section_ids_for(&case.question);
        if case.sections.iter().any(|s| ids.contains(s)) {
            hits += 1;
        } else {
            misses.push(format!("{} -> {ids:?}", case.question));
        }
        if crm_manual::looks_like_how_to(&case.question) {
            how_to += 1;
        } else {
            not_how_to.push(case.question.as_str());
        }
        if mode_for(Some("instructions"), &case.question, true) == Mode::Assistant {
            to_assistant += 1;
            misses.push(format!("{} -> the assistant", case.question));
        }
        extra_sections += ids.len().saturating_sub(4);
        chars += size(&ids);
    }
    let n = about_the_app.len();
    println!("app questions: {n}");
    println!("retrieval hit: {hits}/{n} ({:.1}%)", percent(hits, n));
    println!("how-to detected: {how_to}/{n} ({:.1}%)", percent(how_to, n));
    println!(
        "sent to the assistant (no manual): {to_assistant}/{n} ({:.1}%)",
        percent(to_assistant, n)
    );
    #[allow(clippy::cast_precision_loss)]
    let per_question = (
        extra_sections as f64 / n as f64,
        chars as f64 / n as f64 / 1024.0,
    );
    println!(
        "sections beyond the 4 core, per question: {:.2}; manual text per question: {:.1} KB",
        per_question.0, per_question.1
    );
    for miss in &misses {
        println!("MISS {miss}");
    }
    println!("not how-to by the rules (the model still picks their sections): {not_how_to:?}");
    let mut asked = 0;
    let mut wrongly_asked = Vec::new();
    for case in &about_the_app {
        let asks = matches!(crm_manual::features(&case.question), Some(Features::Ask(_)));
        if asks == case.clarify {
            asked += usize::from(asks);
        } else {
            wrongly_asked.push(format!("{} (asked back: {asks})", case.question));
        }
    }
    println!("asked back as expected: {asked}; not as expected: {wrongly_asked:?}");
    for case in cases.iter().filter(|c| !c.app) {
        let mode = mode_for(Some("instructions"), &case.question, true);
        let expected = if case.assistant {
            Mode::Assistant
        } else {
            Mode::Instructions
        };
        println!(
            "control {:?}: {mode:?} (expected {expected:?})",
            case.question
        );
        assert_eq!(mode, expected, "{}", case.question);
    }

    // What the eval showed when these rules were made: keep it.
    assert!(percent(hits, n) >= 95.0, "retrieval hit {hits}/{n}");
    assert_eq!(to_assistant, 0);
    assert!(percent(how_to, n) >= 55.0, "how-to {how_to}/{n}");
    assert!(wrongly_asked.is_empty());
    for case in about_the_app.iter().filter(|c| c.example) {
        let ids = crm_manual::section_ids_for(&case.question);
        assert!(
            case.sections.iter().any(|s| ids.contains(s)),
            "{}",
            case.question
        );
        assert!(
            crm_manual::looks_like_how_to(&case.question),
            "{}",
            case.question
        );
    }
}

fn session(role: Option<&str>) -> &'static str {
    match role {
        Some("superuser") => sessions::SUPER_ADMIN,
        Some("admin" | "manager") => sessions::ADMIN,
        _ => sessions::REP,
    }
}

/// `(event, data)` of each server-sent event.
fn events(body: &str) -> Vec<(String, String)> {
    body.split("\n\n")
        .filter(|block| !block.trim().is_empty())
        .map(|block| {
            let mut event = "message".to_string();
            let mut data: Vec<&str> = Vec::new();
            for line in block.lines() {
                if let Some(name) = line.strip_prefix("event:") {
                    event = name.trim().to_string();
                } else if let Some(d) = line.strip_prefix("data:") {
                    data.push(d.strip_prefix(' ').unwrap_or(d));
                }
            }
            (event, data.join("\n"))
        })
        .collect()
}

async fn ask(server: &TestServer, session: &str, query: &str, new: bool) -> (String, Vec<String>) {
    let body = server
        .post("/v1/ai/chat")
        .with_session(session)
        .json(&json!({ "query": query, "isNew": new, "mode": "instructions" }))
        .await
        .text();
    let events = events(&body);
    let answer: String = events
        .iter()
        .filter(|(e, d)| e == "message" && !d.starts_with("[DONE]"))
        .map(|(_, d)| d.as_str())
        .collect();
    let clarify: Vec<String> = events
        .iter()
        .filter(|(e, _)| e == "clarify")
        .flat_map(|(_, d)| serde_json::from_str::<Vec<Value>>(d).unwrap_or_default())
        .filter_map(|c| c["label"].as_str().map(String::from))
        .collect();
    (answer, clarify)
}

async fn last_output_tokens(pool: &MySqlPool) -> Option<i64> {
    sqlx::query_scalar(
        "SELECT CAST(output_tokens AS SIGNED) FROM ai_usage_log WHERE feature = 'chat' ORDER BY id DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

/// The model's picks and answers for the eval (`OPEN_AI_SECRET_KEY`).
#[test_db::test(seed)]
#[ignore = "asks the real model"]
async fn the_model_picks_and_answers(pool: MySqlPool) {
    if std::env::var("OPEN_AI_SECRET_KEY").map_or(true, |k| k.trim().is_empty()) {
        println!("OPEN_AI_SECRET_KEY is not set: no model eval");
        return;
    }
    // The superuser of the eval, and an admin with the Manager position.
    sqlx::raw_sql(
        "UPDATE users SET is_superuser = 1 WHERE id = 103;
         INSERT INTO users_positions (user_id, position_id, company_id) VALUES (102, 5, 100);",
    )
    .execute(&pool)
    .await
    .unwrap();
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN).with_ai(Ai::from_env());
    let caller = Caller {
        feature: "chat",
        user_id: Some(100),
        company_id: Some(100),
    };

    // 1. The sections the chat answers from (the clarify table, the
    // model's pick and the manual's own words), for every question.
    let cases = cases();
    let questions: Vec<&Case> = cases.iter().filter(|c| c.app).collect();
    let shared = &state;
    let mut results = Vec::new();
    for chunk in questions.chunks(8) {
        results.extend(
            join_all(chunk.iter().map(|case| async move {
                let started = std::time::Instant::now();
                let picked =
                    crm_manual::sections_for(shared, caller, &case.question, &[], &case.question)
                        .await;
                (*case, picked, started.elapsed())
            }))
            .await,
        );
    }
    let mut hits = 0;
    let mut chars = 0;
    let mut took_all: Vec<std::time::Duration> = Vec::new();
    for (case, picked, took) in &results {
        took_all.push(*took);
        match picked {
            Ok(ids) => {
                chars += size(ids);
                let hit = !case.clarify && case.sections.iter().any(|s| ids.contains(s));
                hits += usize::from(hit);
                if !hit {
                    println!("PICK MISS {} -> {ids:?}", case.question);
                }
            }
            Err(asked) => {
                hits += usize::from(case.clarify);
                let labels: Vec<&str> = asked.choices.iter().map(|c| c.label.as_str()).collect();
                println!(
                    "ASKED {} -> {:?} {labels:?}{}",
                    case.question,
                    asked.question,
                    if case.clarify { "" } else { " (not expected)" }
                );
            }
        }
    }
    took_all.sort();
    let (median, slowest) = (took_all[took_all.len() / 2], took_all[took_all.len() - 1]);
    let n = questions.len();
    #[allow(clippy::cast_precision_loss)]
    let kb = chars as f64 / n as f64 / 1024.0;
    println!(
        "model pick hit: {hits}/{n} ({:.1}%); manual text per question beyond the core: {kb:.1} KB; pick time: median {median:?}, slowest {slowest:?}",
        percent(hits, n)
    );
    let usage: (i64, Option<f64>, Option<f64>, Option<f64>) = sqlx::query_as(
        "SELECT COUNT(*), CAST(AVG(input_tokens) AS DOUBLE), CAST(AVG(cached_input_tokens) AS DOUBLE),
                CAST(AVG(output_tokens) AS DOUBLE) FROM ai_usage_log WHERE feature = 'chat'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    println!("picks: {usage:?} (calls, input, cached input, output tokens on average)");

    // 2. The answers to the questions users reported, as the chat gives them.
    let server = TestServer::new(app(state));
    let mut asked: Vec<(&str, &str)> = questions
        .iter()
        .filter(|c| c.example)
        .map(|c| (session(c.role.as_deref()), c.question.as_str()))
        .collect();
    asked.push((sessions::REP, "How can I connect Telnyx"));
    for (session, question) in asked {
        let (answer, choices) = ask(&server, session, question, true).await;
        let tokens = last_output_tokens(&pool).await;
        println!(
            "\n=== {question} ({session})\n{answer}\nchoices: {choices:?}\noutput tokens: {tokens:?}; words: {}",
            answer.split_whitespace().count()
        );
        assert!(!answer.trim().is_empty(), "{question}: no answer");
        if let Some(choice) = choices.get(1) {
            let (answer, _) = ask(&server, session, choice, false).await;
            let tokens = last_output_tokens(&pool).await;
            println!(
                "\n=== {question} -> {choice}\n{answer}\noutput tokens: {tokens:?}; words: {}",
                answer.split_whitespace().count()
            );
            assert!(tokens.is_some_and(|t| t < 2000), "{choice}: cut off");
        } else {
            assert!(tokens.is_some_and(|t| t < 2000), "{question}: cut off");
        }
    }
}
