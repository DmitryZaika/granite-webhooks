//! The CRM manual chat's eval (`fixtures/crm_manual_eval.json`): realistic
//! questions, typos included, and the manual sections that hold each answer.
//! Run with `--nocapture` for the report. `--ignored` also asks the real
//! model (GitHub Actions has `OPEN_AI_SECRET_KEY`):
//!
//! `gh workflow run "API tests" --ref <branch> -f cargo_args="-p api --test ai crm_manual_eval:: -- --ignored --nocapture"`

use crate::common;

use api::ai::{Ai, Caller, Chat, LARGE_MODEL, MINI_MODEL};
use api::app;
use api::chat::crm_manual::{self, Features};
use api::chat::{Mode, mode_for};
use api::state::AppState;
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use futures_util::future::join_all;
use futures_util::stream::{self, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::time::{Duration, Instant};

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
    serde_json::from_str(include_str!("../fixtures/crm_manual_eval.json")).expect("valid eval")
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
    assert!(percent(how_to, n) >= 50.0, "how-to {how_to}/{n}");
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

/// What the chat sent back for a question: the answer and its events.
struct Answer {
    text: String,
    choices: Vec<String>,
    sources: Vec<String>,
    images: Vec<String>,
    manual: bool,
}

async fn ask(server: &TestServer, session: &str, query: &str, new: bool) -> Answer {
    let body = server
        .post("/v1/ai/chat")
        .with_session(session)
        .json(&json!({ "query": query, "isNew": new, "mode": "instructions" }))
        .await
        .text();
    let events = events(&body);
    let named = |name: &str| -> Vec<Value> {
        events
            .iter()
            .filter(|(e, _)| e == name)
            .flat_map(|(_, d)| serde_json::from_str::<Vec<Value>>(d).unwrap_or_default())
            .collect()
    };
    Answer {
        text: events
            .iter()
            .filter(|(e, d)| e == "message" && !d.starts_with("[DONE]"))
            .map(|(_, d)| d.as_str())
            .collect(),
        choices: named("clarify")
            .iter()
            .filter_map(|c| c["label"].as_str().map(String::from))
            .collect(),
        sources: named("instructions")
            .iter()
            .filter_map(|i| i["title"].as_str().map(String::from))
            .collect(),
        images: named("images")
            .iter()
            .filter_map(|u| u.as_str().map(String::from))
            .collect(),
        manual: events.iter().any(|(e, _)| e == "crmManual"),
    }
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

/// Asks and prints what came back, as the report shows it.
async fn ask_and_show(
    server: &TestServer,
    pool: &MySqlPool,
    session: &str,
    query: &str,
    new: bool,
) -> Answer {
    let answer = ask(server, session, query, new).await;
    println!(
        "\n=== {query} ({session})\n{}\nchoices: {:?}\ninstructions (sources): {:?}\nimages: {:?}\nfrom the manual: {}; output tokens: {:?}; words: {}",
        answer.text,
        answer.choices,
        answer.sources,
        answer.images,
        answer.manual,
        last_output_tokens(pool).await,
        answer.text.split_whitespace().count()
    );
    assert!(!answer.text.trim().is_empty(), "{query}: no answer");
    answer
}

/// Like company 1 (Granite Depot of Indianapolis): handbook pages with
/// pictures that mention quotes, orders and colors, and its special orders.
const COMPANY_ONE: &str = "
INSERT INTO instructions (id, title, rich_text, company_id) VALUES
  (9130, 'Salesrep Handbook - 12.3: Cabinets - Renderings and Moraware quotes',
   '<p>Cabinet renderings: pick the door style and the color option in the rendering tool, then send the rendering to the customer.</p><img src=\"https://img.test/moraware-1.png\"><p>Moraware quotes for cabinets: there is no other option for cabinet colors in Moraware.</p><img src=\"https://img.test/moraware-2.png\">', 100),
  (9131, 'Salesrep Handbook - 12.4: Cabinets - Process: Orders & Schedule',
   '<p>Cabinet orders: once the customer signs, order the cabinets and schedule the install.</p><img src=\"https://img.test/orders.png\">', 100),
  (9132, 'Special orders', '<p>Special orders need a 50% deposit. Order the slabs from the supplier after the deposit is in.</p>', 100);
";

/// Like company 3 (Granite Depot of Columbus): only the AI follow-up
/// instruction, which the chat never reads.
const COMPANY_THREE: &str = "
INSERT INTO instructions (id, title, rich_text, company_id) VALUES
  (9140, 'AI follow-up instruction', '<p>Follow up by email two days after the quote.</p>', 101);
";

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
    sqlx::raw_sql(COMPANY_ONE).execute(&pool).await.unwrap();
    sqlx::raw_sql(COMPANY_THREE).execute(&pool).await.unwrap();
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
                // Not about the app (the model says): the manual's own words, as the chat does.
                let ids = ids
                    .clone()
                    .unwrap_or_else(|| crm_manual::section_ids_for(&case.question));
                chars += size(&ids);
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

    // 2. The questions users reported, as the chat answers them (each a new
    // chat; a question back is answered with its second choice).
    let server = TestServer::new(app(state));
    let mut asked: Vec<(&str, &str)> = questions
        .iter()
        .filter(|c| c.example)
        .map(|c| (session(c.role.as_deref()), c.question.as_str()))
        .collect();
    asked.push((sessions::REP, "How can I connect Telnyx"));
    for (session, question) in asked {
        let answer = ask_and_show(&server, &pool, session, question, true).await;
        assert!(
            answer.sources.is_empty(),
            "{question}: a manual answer has no sources"
        );
        if let Some(choice) = answer.choices.get(1) {
            let picked = ask_and_show(&server, &pool, session, choice, false).await;
            assert!(
                picked.sources.is_empty() && picked.images.is_empty(),
                "{choice}"
            );
        }
    }

    // 3. Company 3 (only the AI follow-up instruction): "How to make a
    // special order?" is not asked about the company's process.
    let columbus = sessions::OTHER_COMPANY_REP;
    let answer = ask_and_show(
        &server,
        &pool,
        columbus,
        "How to make a special order?",
        true,
    )
    .await;
    assert!(
        !answer.text.contains("company's own process"),
        "{}",
        answer.text
    );
    if let Some(choice) = answer.choices.first() {
        let picked = ask_and_show(&server, &pool, columbus, choice, false).await;
        assert!(picked.sources.is_empty() && picked.images.is_empty());
    }

    // 4. Company 1's conversation: asked which special order, the quote, then
    // the follow-ups the user sent; none gets the handbook's pages.
    let indy = sessions::REP;
    let answer = ask_and_show(&server, &pool, indy, "How to make a special order?", true).await;
    assert!(answer.choices.iter().any(|c| c == "Our company's process"));
    ask_and_show(&server, &pool, indy, "Special-order slab in a quote", false).await;
    for follow_up in [
        "they don't have it in the price lists",
        "what does it mean other color?? there is no such option",
        "why are you sending me this page, there is no mention about other color??",
    ] {
        let answer = ask_and_show(&server, &pool, indy, follow_up, false).await;
        assert!(answer.manual, "{follow_up}: stays on the manual");
        assert!(
            answer.sources.is_empty() && answer.images.is_empty(),
            "{follow_up}"
        );
        assert!(
            !answer.text.to_lowercase().contains("other color"),
            "{follow_up}"
        );
    }
    // The company's own process still comes from its instructions.
    ask_and_show(
        &server,
        &pool,
        indy,
        "what is our special order deposit policy",
        false,
    )
    .await;
}

/// How the large model grades a manual answer.
const GRADE_RULES: &str = r#"You check the answers of a help chat for Granite Manager, a CRM for countertop and stone shops. The chat answers "how do I..." questions from the app's user manual. You get the manual text that holds the answer (REFERENCE), who asked, the question, and the chat's answer.

Grade the answer against the REFERENCE only:
- "correct": it does what was asked with places, steps and labels the REFERENCE supports. Other words for the same thing, and optional details left out, are fine.
- "partial": the right feature, but it misses a step the user needs, gets a label, place or role wrong, or buries the answer in things not asked.
- "wrong": the wrong feature or page, steps the REFERENCE contradicts, or buttons, fields, pages or settings the REFERENCE does not have.
- "unanswered": it says it can't confirm, there is no such feature, or asks a question back, while the REFERENCE answers the question.
- "no_reference": the REFERENCE does not answer the question (judge nothing else).
When the REFERENCE says the asker's role can't do it (an admin page asked by a sales rep), a correct answer says who can.

Reply with a JSON object only: {"verdict": "...", "problem": "what is wrong, in one short sentence; empty when correct", "invented": ["labels in the answer that the REFERENCE does not have"]}"#;

fn asker(role: Option<&str>) -> &'static str {
    match role {
        Some("superuser") => "a platform superuser",
        Some("admin" | "manager") => "an admin with the Manager position (no Employee access)",
        _ => "a Sales Rep (Employee access, no Admin)",
    }
}

/// The large model's grade of `answer` (`verdict`, `problem`, `invented`).
async fn grade(state: &AppState, caller: Caller<'_>, case: &Case, answer: &str) -> Value {
    let reference = crm_manual::content_with(&state.pool, &case.sections).await;
    let mut chat = Chat::new(LARGE_MODEL)
        .message("system", GRADE_RULES)
        .message(
            "user",
            &format!(
                "REFERENCE:\n\n{reference}\n\nASKED BY: {}\n\nQUESTION: {}\n\nANSWER:\n{answer}",
                asker(case.role.as_deref()),
                case.question
            ),
        )
        .max_tokens(Some(4000));
    chat.json = true;
    chat.reasoning_effort = Some("low");
    chat.timeout = Duration::from_secs(180);
    let reply = match state.ai.chat(&state.pool, caller, &chat).await {
        Err(err) if err.is_model_permission() => {
            chat.model = MINI_MODEL;
            chat.reasoning_effort = Some("medium");
            state.ai.chat(&state.pool, caller, &chat).await
        }
        other => other,
    };
    match reply {
        Ok(text) => serde_json::from_str(&text)
            .unwrap_or_else(|_| json!({ "verdict": "error", "problem": text })),
        Err(err) => json!({ "verdict": "error", "problem": err.to_string() }),
    }
}

/// Every app question of both fixtures asked as a new chat and graded, one
/// `EVAL {json}` line each, then the totals (`OPEN_AI_SECRET_KEY`):
///
/// `gh workflow run "API tests" --ref <branch> -f cargo_args="-p api --test ai crm_manual_eval::answers_a_lot_of_questions -- --ignored --nocapture"`
#[test_db::test(seed)]
#[ignore = "asks the real model about 550 questions"]
#[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
async fn answers_a_lot_of_questions(pool: MySqlPool) {
    if std::env::var("OPEN_AI_SECRET_KEY").map_or(true, |k| k.trim().is_empty()) {
        println!("OPEN_AI_SECRET_KEY is not set: no model eval");
        return;
    }
    sqlx::raw_sql(
        "UPDATE users SET is_superuser = 1 WHERE id = 103;
         INSERT INTO users_positions (user_id, position_id, company_id) VALUES (102, 5, 100);",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(COMPANY_ONE).execute(&pool).await.unwrap();
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN).with_ai(Ai::from_env());
    let caller = Caller {
        feature: "chat",
        user_id: Some(100),
        company_id: Some(100),
    };
    let more: Vec<Case> =
        serde_json::from_str(include_str!("../fixtures/crm_manual_questions.json"))
            .expect("valid questions");
    let cases: Vec<Case> = more
        .into_iter()
        .chain(cases().into_iter().filter(|c| c.app))
        .collect();
    let server = TestServer::new(app(state.clone()));
    let (server, state) = (&server, &state);
    let results: Vec<Value> = stream::iter(&cases)
        .map(|case| async move {
            let started = Instant::now();
            let answer = ask(server, session(case.role.as_deref()), &case.question, true).await;
            let took = started.elapsed().as_secs_f64();
            let graded = if answer.choices.is_empty() {
                grade(state, caller, case, &answer.text).await
            } else {
                let verdict = if case.clarify { "asked_ok" } else { "asked" };
                json!({ "verdict": verdict })
            };
            let picked =
                match crm_manual::sections_for(state, caller, &case.question, &[], &case.question)
                    .await
                {
                    Ok(ids) => json!(ids),
                    Err(asked) => json!({ "asked": asked.question }),
                };
            let result = json!({
                "question": case.question,
                "role": case.role,
                "expected": case.sections,
                "picked": picked,
                "verdict": graded["verdict"],
                "problem": graded["problem"],
                "invented": graded["invented"],
                "manual": answer.manual,
                "choices": answer.choices,
                "sources": answer.sources,
                "words": answer.text.split_whitespace().count(),
                "seconds": (took * 10.0).round() / 10.0,
                "answer": answer.text,
            });
            println!("EVAL {result}");
            result
        })
        .buffer_unordered(12)
        .collect()
        .await;

    let n = results.len();
    let mut verdicts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for result in &results {
        *verdicts
            .entry(result["verdict"].as_str().unwrap_or("error").to_string())
            .or_default() += 1;
    }
    let words: usize = results
        .iter()
        .filter_map(|r| r["words"].as_u64())
        .map(|w| usize::try_from(w).unwrap_or_default())
        .sum();
    let good = verdicts.get("correct").copied().unwrap_or(0)
        + verdicts.get("asked_ok").copied().unwrap_or(0);
    println!(
        "TOTAL {n} questions: {:.1}% correct; {verdicts:?}; {:.0} words per answer",
        percent(good, n),
        words as f64 / n.max(1) as f64
    );
}
