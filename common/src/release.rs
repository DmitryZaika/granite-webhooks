//! The git commit (`GIT_SHA`, short) the running binary was built from.
//!
//! Each binary's `main` passes in `option_env!("GIT_SHA")`, so a new commit
//! rebuilds only the binary itself, not every crate that reports it.

use std::sync::OnceLock;

static RELEASE: OnceLock<&'static str> = OnceLock::new();

/// Records the build's commit. Only the first call counts.
pub fn set(sha: Option<&'static str>) {
    let _ = RELEASE.set(sha.filter(|sha| !sha.is_empty()).unwrap_or("unknown"));
}

/// The commit `main` recorded, or `unknown` (tests, a build without `GIT_SHA`).
pub fn get() -> &'static str {
    RELEASE.get().copied().unwrap_or("unknown")
}
