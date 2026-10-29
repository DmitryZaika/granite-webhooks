//! Admin and superuser screens, billing, integrations, marketing,
//! questionnaires, delayed sends, the PostHog auto-fix (api-admin).
//!
//! One file's tests: `cargo test -p api --test admin <file>::`.

#[path = "../common/mod.rs"]
mod common;
#[path = "../support/fake_stripe.rs"]
mod fake_stripe;
#[path = "../support/fake_webhooks.rs"]
mod fake_webhooks;

mod billing;
mod billing_addons;
mod billing_amount_notice;
mod billing_cancellation;
mod billing_methods;
mod billing_reminders;
mod billing_seats;
mod billing_setup;
mod billing_stripe_live;
mod bug_reports;
mod checklists;
mod company_create;
mod customer_sources;
mod dashboard;
mod delayed;
mod entitlements;
mod integrations;
mod maintenance;
mod marketing;
mod onboarding_call;
mod onboarding_call_routes;
mod plan_invites;
mod posthog_autofix_callback;
mod posthog_autofix_intake;
mod questionnaires;
mod reports;
mod superuser;
mod superuser_companies;
mod stripe_setup;
mod survey_results;
