//! The tour registries' data the API needs.
//!
//! Generated from the app's `featureTasks.ts` and `onboardingModules.ts` into
//! `api/data/onboarding_manifest.json` (`scripts/export-onboarding-manifest.ts`
//! in the app; a test there fails when it is stale).

use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::LazyLock;

#[derive(Debug, Deserialize)]
pub struct Task {
    pub id: String,
    pub version: i64,
    pub points: i64,
    /// Completed only by the server's own check (no steps to walk).
    pub checklist: bool,
    /// Steps every user must do.
    pub required_steps: usize,
    /// Stored keys of the steps.
    pub base_keys: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub module_ids: Vec<String>,
    pub tasks: Vec<Task>,
    pub retired_task_ids: Vec<String>,
    pub creditable_task_ids: Vec<String>,
}

static MANIFEST: LazyLock<Manifest> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../../../data/onboarding_manifest.json"))
        .expect("onboarding_manifest.json parses")
});

pub fn manifest() -> &'static Manifest {
    &MANIFEST
}

pub fn is_module(id: &str) -> bool {
    MANIFEST.module_ids.iter().any(|module| module == id)
}

pub fn task(id: &str) -> Option<&'static Task> {
    MANIFEST.tasks.iter().find(|task| task.id == id)
}

/// A retired task with no live definition: its events are ignored and its
/// points reprice to zero.
pub fn is_retired(id: &str) -> bool {
    task(id).is_none()
        && MANIFEST
            .retired_task_ids
            .iter()
            .any(|retired| retired == id)
}

pub fn is_creditable(id: &str) -> bool {
    MANIFEST.creditable_task_ids.iter().any(|task| task == id)
}

/// `looksGenuinelyCompleted`: enough of the task's own steps were done.
fn looks_completed(task: &Task, done_steps: &serde_json::Map<String, Value>) -> bool {
    if task.required_steps == 0 {
        return !done_steps.is_empty();
    }
    let base_keys: HashSet<&str> = task.base_keys.iter().map(String::as_str).collect();
    let recorded = done_steps
        .keys()
        .filter(|key| {
            let base = key.rfind('#').map_or(key.as_str(), |at| &key[..at]);
            base_keys.contains(base)
        })
        .count();
    recorded >= task.required_steps
}

/// `registryPriceFor`: the points a completed task is worth. A completion
/// recorded against an older version keeps its price (its steps changed).
pub fn price_for(
    task_id: &str,
    done_steps: &serde_json::Map<String, Value>,
    recorded_version: Option<i64>,
) -> i64 {
    let Some(task) = task(task_id) else {
        return 0;
    };
    if task.checklist {
        return task.points;
    }
    let older = recorded_version.is_some_and(|version| version < task.version);
    if !older && !looks_completed(task, done_steps) {
        return 0;
    }
    task.points
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_manifest_loads() {
        assert!(!manifest().tasks.is_empty());
        assert!(is_module(&manifest().module_ids[0]));
        assert!(!is_module("nope"));
        assert!(is_creditable("setup-email-name"));
    }

    #[test]
    fn prices_need_the_steps_done() {
        let tour = manifest()
            .tasks
            .iter()
            .find(|task| !task.checklist && task.required_steps > 0)
            .unwrap();
        let none = serde_json::Map::new();
        assert_eq!(price_for(&tour.id, &none, Some(tour.version)), 0);
        let mut all = serde_json::Map::new();
        for key in &tour.base_keys {
            all.insert(format!("{key}#1"), json!("action"));
        }
        assert_eq!(price_for(&tour.id, &all, Some(tour.version)), tour.points);
        // Recorded against an older version: kept.
        assert_eq!(
            price_for(&tour.id, &none, Some(tour.version - 1)),
            tour.points
        );
        assert_eq!(price_for("no-such-task", &all, None), 0);
    }
}
