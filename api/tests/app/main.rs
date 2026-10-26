//! The app as a whole: the OpenAPI document, file ownership across routes, the
//! public showroom, the jobs, notifications and the company routes.
//!
//! One file's tests: `cargo test -p api --test app <file>::`.
//!
//! The integration tests of `api` are one test binary per area,
//! `tests/<area>/main.rs`, rather than one per file: after a change in a
//! crate, cargo then links a few binaries instead of ~160.

#[path = "../common/mod.rs"]
mod common;

mod company;
mod db_pool;
mod drive_miles;
mod file_ownership;
mod jobs;
mod notifications;
mod openapi;
mod public;

/// Each file of `tests/<area>/` is a module of that area's binary; a file
/// not listed in its `main.rs` would never run.
#[test]
fn every_test_file_is_a_module() {
    let tests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut missing = Vec::new();
    for area in std::fs::read_dir(&tests).unwrap() {
        let area = area.unwrap().path();
        let Ok(listed) = std::fs::read_to_string(area.join("main.rs")) else {
            continue;
        };
        for file in std::fs::read_dir(&area).unwrap() {
            let path = file.unwrap().path();
            let name = path.file_stem().unwrap().to_string_lossy().into_owned();
            if path.extension().is_some_and(|ext| ext == "rs")
                && name != "main"
                && !listed.contains(&format!("\nmod {name};\n"))
            {
                missing.push(path.strip_prefix(&tests).unwrap().display().to_string());
            }
        }
    }
    missing.sort();
    assert!(
        missing.is_empty(),
        "list these in their tests/<area>/main.rs: {missing:?}"
    );
}
