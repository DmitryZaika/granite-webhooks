//! One module per domain.
//!
//! Each module exposes `router()`, registering every handler with `routes!`
//! so it lands in the OpenAPI spec too. The full path
//! (`/v1/<domain>/...`) lives in the handler's `#[utoipa::path]`, so every
//! route is still greppable from its URL.

pub mod ai;
pub mod ai_usage;
pub mod app_shell;
pub mod attachable;
pub mod auth;
pub mod bug_reports;
pub mod calendars;
pub mod call_coaching;
pub mod calls;
pub mod catalog;
pub mod checklists;
pub mod company;
pub mod created;
pub mod customer_sources;
pub mod customers;
pub mod dashboard;
pub mod deals;
pub mod delayed;
pub mod documents;
pub mod emails;
pub mod events;
pub mod faucets;
pub mod fixtures;
pub mod image_library;
pub mod instructions;
pub mod invoice_queue;
pub mod jobs;
pub mod leaderboard;
pub mod maintenance;
pub mod marketing;
pub mod me;
pub mod notifications;
pub mod onboarding;
pub mod places;
pub mod public;
pub mod questionnaires;
pub mod quickbooks;
pub mod quote_presets;
pub mod quotes;
pub mod reports;
pub mod sales;
pub mod schedule;
pub mod shop;
pub mod sinks;
pub mod slabs;
pub mod sms;
pub mod stats;
pub mod stones;
pub mod superuser;
pub mod supports;
pub mod survey_results;
pub mod teach_mode;
pub mod teach_progress;
pub mod templates;
pub mod transaction_slabs;
pub mod transactions;
pub mod uploads;
pub mod users;

use crate::state::AppState;
use utoipa_axum::router::OpenApiRouter;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .merge(me::router())
        .merge(customers::router())
        .merge(customer_sources::router())
        .merge(users::router())
        .merge(stones::router())
        .merge(sinks::router())
        .merge(faucets::router())
        .merge(slabs::router())
        .merge(sales::router())
        .merge(catalog::router())
        .merge(checklists::router())
        .merge(uploads::router())
        .merge(public::router())
        .merge(company::router())
        .merge(stats::router())
        .merge(dashboard::router())
        .merge(leaderboard::router())
        .merge(deals::router())
        .merge(calendars::router())
        .merge(questionnaires::router())
        .merge(emails::router())
        .merge(templates::router())
        .merge(sms::router())
        .merge(attachable::router())
        .merge(documents::router())
        .merge(image_library::router())
        .merge(supports::router())
        .merge(invoice_queue::router())
        .merge(maintenance::router())
        .merge(marketing::router())
        .merge(quote_presets::router())
        .merge(reports::router())
        .merge(shop::router())
        .merge(survey_results::router())
        .merge(teach_mode::router())
        .merge(teach_progress::router())
        .merge(bug_reports::router())
        .merge(superuser::router())
        .merge(transactions::router())
        .merge(transaction_slabs::router())
        .merge(instructions::router())
        .merge(events::router())
        .merge(schedule::router())
        .merge(places::router())
        .merge(calls::router())
        .merge(quotes::router())
        .merge(delayed::router())
        .merge(ai_usage::router())
        .merge(auth::router())
        .merge(app_shell::router())
        .merge(onboarding::router())
        .merge(ai::router())
        .merge(notifications::router())
        .merge(quickbooks::router())
        .merge(jobs::router())
        .merge(call_coaching::router())
}
