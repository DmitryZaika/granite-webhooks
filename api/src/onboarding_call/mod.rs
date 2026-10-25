//! Onboarding with Dima: the self-serve commitment to complete an onboarding
//! call within 30 days, or pay the onboarding fee (Terms section 11).
//!
//! - `commitment`: the rules, pure
//! - `store`: `company_onboarding_commitments`, its timeline, the daily slot checks
//! - `calendly`: Dima's Calendly (`CALENDLY_API_TOKEN`, `CALENDLY_BASE_URL`)
//! - `job`: `POST /v1/jobs/onboarding-commitments`
//! - `emails`: reminders and the fee notice
//! - `fee`: the fee invoice in Stripe and its webhook events

pub mod calendly;
pub mod commitment;
pub mod emails;
pub mod fee;
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
    /// `ONBOARDING_FEE_CHARGING=1`: a passed deadline leads to the fee
    /// notice and invoice; off, it ends as `fee_skipped`.
    pub fee_charging: bool,
}

impl Config {
    pub fn disabled() -> Self {
        Self {
            calendly: Calendly::disabled(),
            fee_charging: false,
        }
    }

    pub fn from_env() -> Self {
        Self {
            calendly: Calendly::from_env(),
            fee_charging: std::env::var("ONBOARDING_FEE_CHARGING")
                .is_ok_and(|value| value.trim() == "1"),
        }
    }
}
