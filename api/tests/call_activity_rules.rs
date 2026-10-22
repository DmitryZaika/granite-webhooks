//! The Rust call activity rules give what the Remix TypeScript gave on a
//! corpus of transcripts, model answers and call times
//! (`fixtures/call_activity_cases.json`, generated from the TypeScript before
//! it was deleted; 50,000 random cases matched as well).

use api::routes::ai::call_activity_rules::{
    CallActivity, finalize, from_response, voicemail_follow_up_deadline,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct Case {
    transcript: String,
    committed_at: String,
    response: Value,
    expected: Vec<CallActivity>,
    voicemail_rules: Vec<Option<String>>,
    finalized: Vec<CallActivity>,
}

#[test]
fn the_rules_match_the_typescript() {
    compare(include_str!("fixtures/call_activity_cases.json"));
}

/// A larger corpus generated on demand:
/// `CALL_ACTIVITY_CORPUS=/path/to/cases.json cargo test -- --ignored`.
#[test]
#[ignore = "needs a generated corpus"]
fn the_rules_match_the_typescript_on_a_generated_corpus() {
    let path = std::env::var("CALL_ACTIVITY_CORPUS").expect("CALL_ACTIVITY_CORPUS is set");
    compare(&std::fs::read_to_string(path).expect("corpus readable"));
}

fn compare(json: &str) {
    let cases: Vec<Case> = serde_json::from_str(json).expect("fixture parses");
    let mut failures = Vec::new();
    for (index, case) in cases.iter().enumerate() {
        let committed: DateTime<Utc> = case.committed_at.parse().expect("time");
        let got = from_response(&case.response, committed, &case.transcript);
        if got != case.expected {
            failures.push(format!(
                "#{index} {}\n  response {}\n  at {}\n  want {:?}\n  got  {:?}",
                case.transcript, case.response, case.committed_at, case.expected, got
            ));
            continue;
        }
        let rules: Vec<Option<String>> = got
            .iter()
            .map(|activity| {
                voicemail_follow_up_deadline(
                    activity.deadline.as_deref(),
                    &activity.name,
                    committed,
                )
            })
            .collect();
        if rules != case.voicemail_rules {
            failures.push(format!(
                "#{index} voicemail rules: want {:?} got {rules:?}",
                case.voicemail_rules
            ));
        }
        let finalized = finalize(
            vec![CallActivity {
                name: "Follow-up to confirm interest".into(),
                deadline: None,
            }],
            committed,
            &case.transcript,
        );
        if finalized != case.finalized {
            failures.push(format!(
                "#{index} {}\n  finalized: want {:?} got {finalized:?}",
                case.transcript, case.finalized
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases differ:\n{}",
        failures.len(),
        cases.len(),
        failures
            .iter()
            .take(12)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}
