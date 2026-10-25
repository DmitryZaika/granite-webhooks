//! One module per domain.
//!
//! Each module exposes `router()`, registering every handler with `routes!`
//! so it lands in the OpenAPI spec too. The full path
//! (`/v1/<domain>/...`) lives in the handler's `#[crate::openapi::path]`, so every
//! route is still greppable from its URL.

pub mod ai;
pub mod ai_usage;
pub mod app_shell;
pub mod attachable;
pub mod auth;
pub mod billing;
pub mod billing_account;
pub mod bug_reports;
pub mod calendars;
pub mod call_coaching;
pub mod calls;
pub mod catalog;
pub mod checklists;
pub mod commissions;
pub mod company;
pub mod company_frame;
pub mod created;
pub mod customer_sources;
pub mod customer_view;
pub mod customers;
pub mod dashboard;
pub mod deals;
pub mod delayed;
pub mod device_sessions;
pub mod documents;
pub mod emails;
pub mod events;
pub mod faucets;
pub mod fixtures;
pub mod followups;
pub mod image_library;
pub mod instructions;
pub mod integrations;
pub mod invoice_queue;
pub mod jobs;
pub mod leaderboard;
pub mod maintenance;
pub mod manual_feedback_reviews;
pub mod marketing;
pub mod me;
pub mod notifications;
pub mod onboarding;
pub mod online_quotes;
pub mod phone;
pub mod places;
pub mod posthog_autofix;
pub mod public;
pub mod questionnaires;
pub mod quickbooks;
pub mod quote_editor;
pub mod quote_extras;
pub mod quote_forms;
pub mod quote_lookups;
pub mod quote_portal;
pub mod quote_presets;
pub mod quote_price_lists;
pub mod quotes;
pub mod reports;
pub mod sales;
pub mod schedule;
pub mod shop;
pub mod signature_requests;
pub mod signatures;
pub mod signing;
pub mod signwell_webhook;
pub mod sinks;
pub mod slabs;
pub mod sms;
pub mod stats;
pub mod stones;
pub mod superuser;
pub mod superuser_companies;
pub mod supplier_files;
pub mod suppliers;
pub mod supports;
pub mod survey_results;
pub mod teach_mode;
pub mod teach_progress;
pub mod telnyx_webhooks;
pub mod templates;
pub mod transaction_slabs;
pub mod transactions;
pub mod uploads;
pub mod users;

use crate::openapi::OpenApiRouter;
use crate::state::AppState;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .merge(me::router())
        .merge(customers::router())
        .merge(customer_sources::router())
        .merge(customer_view::router())
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
        .merge(quote_extras::router())
        .merge(quote_editor::router())
        .merge(quote_lookups::router())
        .merge(quote_portal::router())
        .merge(online_quotes::router())
        .merge(commissions::router())
        .merge(company_frame::router())
        .merge(quote_forms::router())
        .merge(quote_price_lists::router())
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
        .merge(followups::router())
        .merge(supplier_files::router())
        .merge(suppliers::router())
        .merge(billing::router())
        .merge(billing_account::router())
        .merge(integrations::router())
        .merge(superuser_companies::router())
        .merge(manual_feedback_reviews::router())
        .merge(signature_requests::router())
        .merge(signing::router())
        .merge(signatures::router())
        .merge(signwell_webhook::router())
        .merge(posthog_autofix::router())
        .merge(device_sessions::router())
        .merge(phone::router())
        .merge(telnyx_webhooks::router())
}
