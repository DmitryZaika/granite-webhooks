//! Onboarding with Dima: the self-serve commitment to complete the (free)
//! onboarding call within 30 days, or pay the next month in advance with the
//! next subscription charge (Terms section 11).
//!
//! - `commitment`: the rules, pure
//! - `store`: `company_onboarding_commitments`, its timeline, the daily slot checks
//! - `calendly`: Dima's Calendly (`CALENDLY_API_TOKEN`, `CALENDLY_BASE_URL`)
//! - `job`: `POST /v1/jobs/onboarding-commitments`
//! - `emails`: reminders and the advance notice
//! - `advance`: the advance in Stripe (invoice item, credit) and its webhook

pub mod advance;
pub mod calendly;
pub mod commitment;
pub mod emails;
pub mod job;
pub mod store;

use calendly::Calendly;

/// `utm_campaign` of every booking made for the onboarding call.
pub const UTM_CAMPAIGN: &str = "granite-onboarding";

/// `utm_content` of the company's bookings: `company-<id>`.
pub fn utm_content(company_id: i32) -> String {
    format!("company-{company_id}")
}

/// The onboarding call's settings.
#[derive(Clone)]
pub struct Config {
    /// Dima's Calendly (`CALENDLY_API_TOKEN`, `CALENDLY_BASE_URL`,
    /// `CALENDLY_ONBOARDING_URL`).
    pub calendly: Calendly,
    /// `ONBOARDING_ADVANCE_CHARGING=1`: a passed deadline leads to the
    /// advance notice and charge; off, it ends as `advance_skipped`.
    pub advance_charging: bool,
}

impl Config {
    pub fn disabled() -> Self {
        Self {
            calendly: Calendly::disabled(),
            advance_charging: false,
        }
    }

    pub fn from_env() -> Self {
        Self {
            calendly: Calendly::from_env(),
            advance_charging: crate::env_check::flag_on("ONBOARDING_ADVANCE_CHARGING"),
        }
    }
}
