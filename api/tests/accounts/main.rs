//! Sign-in, the signed-in user, the company's users, onboarding, teach mode,
//! the leaderboard (api-accounts).
//!
//! One file's tests: `cargo test -p api --test accounts <file>::`.

#[path = "../common/mod.rs"]
mod common;
#[path = "../support/fake_stripe.rs"]
mod fake_stripe;
#[path = "../support/fake_webhooks.rs"]
mod fake_webhooks;

mod app_shell;
mod auth;
mod auth_routes;
mod leaderboard;
mod manage_users;
mod me;
mod me_account;
mod onboarding;
mod position_sections;
mod signup;
mod signup_bank;
mod signup_provisioning;
mod teach_mode;
mod todos;
mod user_call_activity;
mod users;
mod users_activity;
