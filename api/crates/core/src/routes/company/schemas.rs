use crate::openapi::ToSchema;
use serde::Serialize;
use sqlx::FromRow;

/// What the company the request acts in has switched on or set up. Pages use
/// it to decide which features, widgets and links to offer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct CompanyFeatures {
    /// The QuickBooks feature is switched on for the company (on unless a
    /// platform admin turned it off) and, on a subscription plan, the
    /// company has the QuickBooks add-on.
    pub quickbooks_enabled: bool,
    /// The company has connected a QuickBooks account (OAuth tokens saved).
    /// QuickBooks data is only usable when both this and `quickbooks_enabled`
    /// are true.
    pub quickbooks_connected: bool,
    /// Guided tours and their points (the company's own switch, else the
    /// platform-wide default).
    pub onboarding: bool,
    /// The points leaderboard (the company's own switch, else the
    /// platform-wide default). See `GET /v1/points-leaderboard`.
    pub leaderboard: bool,
    /// E-signatures: the company has a SignWell API key (or its own switch is
    /// on) and, on a subscription plan, the Signatures add-on. Each user still
    /// needs their own `is_signature_available` flag.
    pub e_signature: bool,
    /// Quotes: the company's plan includes the Quotes add-on (always true on a
    /// legacy plan).
    pub quotes: bool,
    /// The users' email signature block: the company's plan includes the
    /// Signatures add-on (always true on a legacy plan).
    pub email_signature: bool,
    /// Inbound email is set up (the company has a mail domain): the Emails
    /// inbox and email widgets are offered.
    pub inbound_email: bool,
    /// The company has CloudTalk credentials (calls and texts).
    pub cloudtalk: bool,
    /// The company is connected to RingCentral (OAuth, or JWT credentials).
    /// When both are set up, texts go through RingCentral.
    pub ringcentral: bool,
    /// Granite Phone is switched on (`telnyx_company_config.enabled`): calls
    /// through Telnyx, and texts from the company's Telnyx numbers.
    pub telnyx: bool,
    /// Texting through Telnyx is on: Granite Phone is on and the company's
    /// Telnyx messaging profile is set. Users with a Telnyx line text through
    /// it (`/v1/sms/telnyx/...`).
    pub telnyx_sms: bool,
    /// Marking a deal won or a quote sold opens the sale form, filled from
    /// the quote (Admin › Automations, `PUT /v1/company/auto-sale`).
    pub auto_sale: bool,
    /// Marking a quote sold adds its template and installation to the
    /// schedule's Unscheduled list (Admin › Automations,
    /// `PUT /v1/company/auto-unscheduled-jobs`).
    pub auto_unscheduled_jobs: bool,
}
