//! One module per domain.
//!
//! Each module exposes `router()`, registering every handler with `routes!`
//! so it lands in the OpenAPI spec too. The full path
//! (`/v1/<domain>/...`) lives in the handler's `#[crate::openapi::path]`, so every
//! route is still greppable from its URL.

// The route modules of every crate below (each crate's `routes` re-exports
// the ones below it).
pub use api_admin::routes::*;
pub use api_ai::routes::*;
pub use api_phone::routes::*;

pub mod jobs;
pub mod notifications;
pub mod public;

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
        .merge(position_sections::router())
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
