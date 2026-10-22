//! Writing follow-ups with the model (`followups::generation`) against an
//! OpenAI stand-in: the prompts sent, the repair of copy that fails the
//! checks, and rewriting one message.

mod common;

use api::ai::Ai;
use api::followups::cadence::schedule::plan_ai_followup_cadence;
use api::followups::cadence::types::{PlanAiFollowupCadenceOptions, PlannedAiCadenceStep};
use api::followups::dates::{Instant, parse_iso};
use api::followups::gather::{FollowupRequest, GatheredContext, gather_followup_context};
use api::followups::generation::{
    CopyGenerationMode, RegenerateFailure, Writer, complete_cadence, complete_followup,
    regenerate_cadence_message,
};
use api::followups::pipeline::PipelineTimer;
use api::followups::prompt::GeneratedFollowup;
use api::followups::send_window::FollowupChannel;
use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Replies in order; records each request.
#[derive(Clone, Default)]
struct Model {
    replies: Arc<Mutex<VecDeque<Value>>>,
    requests: Arc<Mutex<Vec<Value>>>,
}

async fn complete(State(model): State<Model>, Json(request): Json<Value>) -> Json<Value> {
    model.requests.lock().unwrap().push(request);
    let content = model
        .replies
        .lock()
        .unwrap()
        .pop_front()
        .expect("a scripted reply");
    Json(json!({
        "model": "gpt-6.1-sol-2026",
        "choices": [{ "message": { "content": content.to_string() }, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 1200, "completion_tokens": 80 }
    }))
}

async fn stub_model(replies: Vec<Value>) -> (Ai, Model) {
    let model = Model {
        replies: Arc::new(Mutex::new(replies.into())),
        requests: Arc::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    let router = Router::new()
        .route("/v1/chat/completions", post(complete))
        .with_state(model.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (Ai::new(Some("sk-test"), &url), model)
}

impl Model {
    fn user_prompts(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| r["messages"][1]["content"].as_str().unwrap().to_string())
            .collect()
    }

    fn system_prompts(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| r["messages"][0]["content"].as_str().unwrap().to_string())
            .collect()
    }
}

fn now() -> Instant {
    parse_iso("2026-10-13T15:00:00Z").unwrap()
}

/// A lead nobody has written to yet (an unknown phone): a first contact.
async fn lead(pool: &MySqlPool) -> (FollowupRequest, GatheredContext, Vec<PlannedAiCadenceStep>) {
    let mut request = FollowupRequest::new(100, 100, FollowupChannel::Sms);
    request.phone_digits = Some("3175559876".into());
    let gathered = gather_followup_context(pool, &request, now())
        .await
        .unwrap();
    let options = PlanAiFollowupCadenceOptions {
        mix_sms_and_email: false,
        channel: Some(FollowupChannel::Sms),
        outbound_count: None,
        first_outbound_at: None,
        hold_until_at: None,
        customer_requested_followup_at: None,
        customer_state: None,
        unanswered_total: None,
        contact_pressure: Some(gathered.contact_pressure.clone()),
    };
    let steps = plan_ai_followup_cadence(None, now(), &options)
        .into_iter()
        .take(3)
        .collect();
    (request, gathered, steps)
}

fn writer<'a>(pool: &'a MySqlPool, ai: &'a Ai) -> Writer<'a> {
    Writer {
        pool,
        ai,
        company_id: 100,
        user_id: 100,
    }
}

fn message(body: &str) -> Value {
    json!({ "skip": false, "subject": "", "body": body, "delayMinutes": 1440 })
}

#[test_db::test(seed)]
async fn new_context_repairs_only_the_failing_message(pool: MySqlPool) {
    let (request, gathered, steps) = lead(&pool).await;
    assert_eq!(steps.len(), 3);
    let (ai, model) = stub_model(vec![
        json!({ "skip": false, "messages": [
            message("Hi! Are you still planning your project?"),
            message("Have you seen our Taj Mahal slabs?"),
            message("Would a quick call help you plan the project?"),
        ]}),
        json!({ "skip": false, "messages": [message("What look are you going for?")] }),
    ])
    .await;
    let mut timer = PipelineTimer::new();
    let messages = complete_cadence(
        writer(&pool, &ai),
        &request,
        &gathered,
        &steps,
        CopyGenerationMode::NewContext,
        now(),
        &mut timer,
    )
    .await
    .unwrap();
    let bodies: Vec<&str> = messages.iter().map(|m| m.body.as_str()).collect();
    assert_eq!(
        bodies,
        [
            "Hi! Are you still planning your project?",
            "What look are you going for?",
            "Would a quick call help you plan the project?"
        ]
    );
    assert_eq!(messages[0].delay_minutes, Some(1440.0));
    let prompts = model.user_prompts();
    assert_eq!(prompts.len(), 2);
    assert!(
        prompts[0].starts_with("CUSTOMER\nfirstName: unknown"),
        "{}",
        prompts[0]
    );
    assert!(prompts[0].contains("STEP 3\nTASK\nchannel: sms"));
    assert!(prompts[1].contains("REGENERATE ONLY the failed messages listed below. Return exactly 1 message, for STEP 2 in that order"));
    assert!(prompts[1].contains(
        "- Message 2: state/copy inconsistency: material Taj Mahal not in structured state"
    ));
    assert!(model.system_prompts()[0].ends_with(
        "top-level {\"skip\":true,\"reason\":\"...\"} only if you cannot write any of the steps"
    ));
    assert_eq!(timer.retry_mode, Some("failed_step_only"));
    assert_eq!(
        timer.llm_primary.as_ref().unwrap().model,
        "gpt-6.1-sol-2026"
    );
    assert_eq!(timer.llm_retry.as_ref().unwrap().input_tokens, Some(1200));
    let logged: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ai_usage_log WHERE feature = 'ai_followup' AND user_id = 100",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(logged, 2);
}

#[test_db::test(seed)]
async fn legacy_regenerates_the_cadence_and_keeps_accepted_messages(pool: MySqlPool) {
    let (request, gathered, steps) = lead(&pool).await;
    let (ai, model) = stub_model(vec![
        json!({ "skip": false, "messages": [message("First text"), message("Have you seen our Taj Mahal slabs?"), message("Third text SKIP")] }),
        json!({ "skip": false, "messages": [message("Changed first"), message("A new second text"), message("Changed third")] }),
    ])
    .await;
    let mut timer = PipelineTimer::new();
    let messages = complete_cadence(
        writer(&pool, &ai),
        &request,
        &gathered,
        &steps,
        CopyGenerationMode::Legacy,
        now(),
        &mut timer,
    )
    .await
    .unwrap();
    // Accepted messages stay; a SKIP placeholder is dropped.
    assert_eq!(messages[0].body, "First text");
    assert_eq!(messages[1].body, "A new second text");
    assert!(messages[2].skip);
    assert_eq!(messages[2].reason.as_deref(), Some("placeholder"));
    let prompts = model.user_prompts();
    assert!(
        prompts[0].starts_with("Write 3 SMS follow-ups for an automated no-response sequence."),
        "{}",
        prompts[0]
    );
    assert!(prompts[1].contains("REGENERATE: fix copy validation issues. Copy validation: Message 2: state/copy inconsistency"));
    assert!(model.system_prompts()[0].starts_with("# Cadence follow-ups (no-response sequence)"));
    assert_eq!(timer.retry_reasons, ["copy_validation"]);
    assert!(timer.prompt_components.unwrap().estimated_input_tokens > 1000);
}

#[test_db::test(seed)]
async fn a_forced_cadence_retries_a_skip_and_nothing_is_written_when_not_allowed(pool: MySqlPool) {
    let (mut request, gathered, steps) = lead(&pool).await;
    request.force_generate = true;
    let (ai, model) = stub_model(vec![
        json!({ "skip": true, "reason": "customer will reach out" }),
        json!({ "skip": false, "messages": [message("One"), message("Two"), message("Three")] }),
    ])
    .await;
    let messages = complete_cadence(
        writer(&pool, &ai),
        &request,
        &gathered,
        &steps,
        CopyGenerationMode::NewContext,
        now(),
        &mut PipelineTimer::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        messages.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(),
        ["One", "Two", "Three"]
    );
    let prompts = model.user_prompts();
    assert!(prompts[1].ends_with("REGENERATE: the salesperson chose Generate anyway. Do not return skip=true. Write every requested message as a low-pressure follow-up."));
    assert!(model.system_prompts()[0].contains("SALESPERSON OVERRIDE"));

    // A customer whose deal is won: no cadence, no model call.
    let mut won = FollowupRequest::new(100, 100, FollowupChannel::Sms);
    won.customer_id = Some(1000);
    let gathered = gather_followup_context(&pool, &won, now()).await.unwrap();
    let (ai, model) = model_none().await;
    let messages = complete_cadence(
        writer(&pool, &ai),
        &won,
        &gathered,
        &steps,
        CopyGenerationMode::Legacy,
        now(),
        &mut PipelineTimer::new(),
    )
    .await
    .unwrap();
    assert!(messages.is_empty());
    assert!(model.user_prompts().is_empty());
}

async fn model_none() -> (Ai, Model) {
    stub_model(Vec::new()).await
}

#[test_db::test(seed)]
async fn one_flow_message_is_cut_to_the_sms_limit_or_skipped(pool: MySqlPool) {
    let (request, gathered, _) = lead(&pool).await;
    let long = "x".repeat(2000);
    let (ai, model) = stub_model(vec![json!({ "skip": false, "subject": "", "body": long })]).await;
    let GeneratedFollowup::Message { body, subject, .. } =
        complete_followup(writer(&pool, &ai), &request, &gathered)
            .await
            .unwrap()
    else {
        panic!("a message");
    };
    assert_eq!(body.len(), 1600);
    assert_eq!(subject, None);
    assert!(
        model.user_prompts()[0]
            .starts_with("Write the next SMS follow-up for automated flow step #1.")
    );
    assert!(model.system_prompts()[0].starts_with("# Primary Goal of Every Follow-Up"));
}

#[test_db::test(seed)]
async fn a_rewrite_must_differ_from_the_rejected_drafts(pool: MySqlPool) {
    let (_, gathered, steps) = lead(&pool).await;
    let rejected = "Hi! Is your kitchen project still on for this fall?";
    let (ai, model) = stub_model(vec![
        json!({ "skip": false, "messages": [message("Hi! Is your kitchen project still on for this fall?")] }),
        json!({ "skip": false, "messages": [message("Would a quick call on Thursday help you pick a direction?")] }),
    ])
    .await;
    let rewritten = regenerate_cadence_message(
        writer(&pool, &ai),
        &gathered,
        &steps[0],
        rejected,
        None,
        &[],
        false,
        now(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        rewritten.0,
        "Would a quick call on Thursday help you pick a direction?"
    );
    let prompts = model.user_prompts();
    assert!(
        prompts[0].contains("REGENERATE THIS SINGLE MESSAGE.")
            && prompts[0].contains(&format!("Rejected body 1:\n\"\"\"{rejected}\"\"\""))
    );
    assert!(prompts[1].contains("Previous attempt was still too similar."));

    let (ai, _) = stub_model(vec![
        json!({ "skip": false, "messages": [message(rejected)] }),
        json!({ "skip": false, "messages": [message(rejected)] }),
        json!({ "skip": false, "messages": [message(rejected)] }),
    ])
    .await;
    let failed = regenerate_cadence_message(
        writer(&pool, &ai),
        &gathered,
        &steps[0],
        rejected,
        None,
        &[],
        false,
        now(),
    )
    .await
    .unwrap();
    assert_eq!(failed, Err(RegenerateFailure::TooSimilar));
}
