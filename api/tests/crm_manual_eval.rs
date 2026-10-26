//! The CRM manual chat's eval (`fixtures/crm_manual_eval.json`): realistic
//! questions, typos included, and the manual sections that hold each answer.
//! Run with `--nocapture` for the report.

use api::chat::crm_manual;
use api::chat::{Mode, mode_for};
use serde::Deserialize;

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
}

const fn yes() -> bool {
    true
}

fn cases() -> Vec<Case> {
    serde_json::from_str(include_str!("fixtures/crm_manual_eval.json")).expect("valid eval")
}

fn percent(part: usize, whole: usize) -> f64 {
    100.0 * part as f64 / whole.max(1) as f64
}

#[test]
fn retrieval_and_routing_report() {
    let cases = cases();
    let app: Vec<&Case> = cases.iter().filter(|c| c.app).collect();
    let mut hits = 0;
    let mut how_to = 0;
    let mut to_assistant = 0;
    let mut extra_sections = 0;
    let mut chars = 0;
    let mut misses = Vec::new();
    for case in &app {
        let ids = crm_manual::section_ids_for(&case.question);
        let hit = case.sections.iter().any(|s| ids.contains(s));
        if hit {
            hits += 1;
        } else {
            misses.push(format!("{} -> {ids:?}", case.question));
        }
        if crm_manual::looks_like_how_to(&case.question) {
            how_to += 1;
        }
        if mode_for(Some("instructions"), &case.question, true) == Mode::Assistant {
            to_assistant += 1;
            misses.push(format!("{} -> the assistant", case.question));
        }
        extra_sections += ids.len().saturating_sub(4);
        chars += ids
            .iter()
            .filter_map(|id| crm_manual::section(id))
            .map(str::len)
            .sum::<usize>();
    }
    let n = app.len();
    println!("app questions: {n}");
    println!("retrieval hit: {hits}/{n} ({:.1}%)", percent(hits, n));
    println!("how-to detected: {how_to}/{n} ({:.1}%)", percent(how_to, n));
    println!(
        "sent to the assistant (no manual): {to_assistant}/{n} ({:.1}%)",
        percent(to_assistant, n)
    );
    println!(
        "sections beyond the 4 core, per question: {:.2}; manual text per question: {:.1} KB",
        extra_sections as f64 / n as f64,
        chars as f64 / n as f64 / 1024.0
    );
    for miss in &misses {
        println!("MISS {miss}");
    }
    for case in cases.iter().filter(|c| !c.app) {
        let mode = mode_for(Some("instructions"), &case.question, true);
        println!(
            "control {:?}: {mode:?} (expected {})",
            case.question,
            if case.assistant {
                "Assistant"
            } else {
                "Instructions"
            }
        );
    }
}
