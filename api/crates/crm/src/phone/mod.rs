//! Granite Phone (business calling through Telnyx): the shared logic.
//!
//! Used by the phone routes (`routes/phone/`), Telnyx's webhooks
//! (`routes/telnyx_webhooks/`), the device sign-in
//! (`routes/device_sessions.rs`) and the phone job.
//!
//! - [`numbers`]: E.164, last 10 digits, US/Canada and N11 checks, digits
//!   for text to speech
//! - [`revoke`]: signing phones out
//! - [`company`]: a company's Telnyx setup (`telnyx_company_config`)
//! - [`clock`]: local days, weeks and months in a company's time zone
//! - [`availability`]: working hours, Do not disturb, who can ring now
//! - [`lines`]: the company's numbers, caller IDs
//! - [`settings`]: a user's phone settings and accepted notices
//! - [`call_rules`]: incoming call rules and "Forward all calls"
//! - [`blocking`]: the company's spam and blocked numbers
//! - [`hold_music`]: what callers hear on hold
//! - [`devices`]: phones, their Telnyx credentials, our pushes
//! - [`limits`]: how many calls a user may start
//! - [`contacts`]: the contact index (customers and coworkers)
//! - [`customer_index`]: customers by phone number, for matching calls
//! - [`calls`]: the call log behind Recents
//! - [`call_log`]: call-log writes as Telnyx reports a call's progress
//! - [`routing`]: who an incoming call rings, and what follows no answer
//! - [`events`]: Telnyx's callbacks, each claimed once
//! - [`recordings`]: copying recordings to our private bucket
//! - [`outcomes`]: our pushes about calls, and the CRM follow-through
//! - [`alerts`]: 911 alerts to the company's admins
//! - [`crm_push`]: pushes of what Telegram also sends (CRM notifications,
//!   lead alerts, the maintenance digest)
//! - [`porting`]: moving existing numbers to Telnyx (port-in orders)
//! - [`texting`]: texting through Telnyx (the `telnyx` SMS provider)
//! - [`texts`]: the app's Texts tab (threads per line and other number)
//! - [`tendlc`]: the 10DLC texting registration form (brand and campaign)
//! - [`tendlc_registry`]: sending it to Telnyx and following it
//! - [`tendlc_chat`]: the AI helper beside the form

pub mod alerts;
pub mod availability;
pub mod blocking;
pub mod call_log;
pub mod call_rules;
pub mod calls;
pub mod clock;
pub mod company;
pub mod contacts;
pub mod crm_push;
pub mod customer_index;
pub mod devices;
pub mod events;
pub mod hold_music;
pub mod limits;
pub mod lines;
pub mod numbers;
pub mod outcomes;
pub mod porting;
pub mod recordings;
pub mod revoke;
pub mod routing;
pub mod settings;
pub mod tendlc;
pub mod tendlc_chat;
pub mod tendlc_check;
pub mod tendlc_pages;
pub mod tendlc_registry;
pub mod texting;
pub mod texts;

pub use crate::phone_config::{DEFAULT_FALLBACK_TEXML_URL, DEFAULT_PUBLIC_API_URL, PhoneConfig};

/// Where "Report a problem" goes, and what the app shows as the support
/// address.
pub const SUPPORT_EMAIL: &str = "support@granite-manager.com";

/// A stored value that is none of an enum's values.
#[derive(Debug, thiserror::Error)]
#[error("unknown value")]
pub struct UnknownValue;

/// A string enum of the phone API: serde and OpenAPI names, `as_str` (as
/// stored), `parse`, and `TryFrom<String>` for `#[sqlx(try_from)]`.
#[macro_export]
macro_rules! wire_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $( $(#[$variant_meta:meta])* $variant:ident = $text:literal, )+
        }
    ) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, $crate::openapi::ToSchema,
        )]
        pub enum $name {
            $( $(#[$variant_meta])* #[serde(rename = $text)] $variant, )+
        }

        impl $name {
            /// As stored and sent.
            #[allow(dead_code)]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $( Self::$variant => $text, )+
                }
            }

            /// The value named `text`, if any.
            pub fn parse(text: &str) -> Option<Self> {
                match text {
                    $( $text => Some(Self::$variant), )+
                    _ => None,
                }
            }
        }

        impl TryFrom<String> for $name {
            type Error = $crate::phone::UnknownValue;

            fn try_from(value: String) -> Result<Self, $crate::phone::UnknownValue> {
                Self::parse(&value).ok_or($crate::phone::UnknownValue)
            }
        }
    };
}
pub use crate::wire_enum;

/// Whether app version `version` is older than `minimum` (both `1.4.0`
/// style; a missing part counts as 0, anything after `-` or `+` is ignored).
/// An unreadable version is never "older".
pub fn older_than(version: &str, minimum: &str) -> bool {
    fn parts(version: &str) -> Option<Vec<u64>> {
        let core = version
            .trim()
            .trim_start_matches(['v', 'V'])
            .split(['-', '+'])
            .next()?;
        if core.is_empty() {
            return None;
        }
        core.split('.').map(|part| part.parse().ok()).collect()
    }
    let (Some(mut version), Some(mut minimum)) = (parts(version), parts(minimum)) else {
        return false;
    };
    let len = version.len().max(minimum.len());
    version.resize(len, 0);
    minimum.resize(len, 0);
    version < minimum
}

#[cfg(test)]
mod tests {
    use super::older_than;

    #[test]
    fn app_versions_compare_by_number() {
        assert!(older_than("1.4.0", "1.10.0"));
        assert!(older_than("1.4", "1.4.1"));
        assert!(older_than("v0.9.9-beta.1", "1.0"));
        assert!(!older_than("1.10.0", "1.4.0"));
        assert!(!older_than("1.4.0", "1.4"));
        assert!(!older_than("2.0.0+build.7", "2.0.0"));
        // Unreadable versions never block anyone.
        assert!(!older_than("", "1.0.0"));
        assert!(!older_than("dev", "1.0.0"));
        assert!(!older_than("1.0.0", "soon"));
    }
}
