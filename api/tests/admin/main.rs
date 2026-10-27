//! Admin and superuser screens, billing, integrations, marketing,
//! questionnaires, delayed sends, the PostHog auto-fix (api-admin).
//!
//! One file's tests: `cargo test -p api --test admin <file>::`.

#[path = "../common/mod.rs"]
mod common;

mod billing;
mod billing_stripe_live;
mod bug_reports;
mod checklists;
mod customer_sources;
mod dashboard;
mod delayed;
mod integrations;
mod maintenance;
mod marketing;
mod posthog_autofix_callback;
mod posthog_autofix_intake;
mod questionnaires;
mod reports;
mod superuser;
mod superuser_companies;
mod survey_results;
