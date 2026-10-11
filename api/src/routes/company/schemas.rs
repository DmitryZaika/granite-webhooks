use serde::Serialize;
use sqlx::FromRow;
use utoipa::ToSchema;

/// What the company the request acts in has switched on or set up. Pages use
/// it to decide which features, widgets and links to offer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct CompanyFeatures {
    /// The QuickBooks feature is switched on for the company (on unless a
    /// platform admin turned it off).
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
    /// E-signatures: the company has a SignWell API key. Each user still needs
    /// their own `is_signature_available` flag.
    pub e_signature: bool,
    /// Inbound email is set up (the company has a mail domain): the Emails
    /// inbox and email widgets are offered.
    pub inbound_email: bool,
    /// The company has CloudTalk credentials (calls and texts).
    pub cloudtalk: bool,
    /// The company is connected to RingCentral (OAuth, or JWT credentials).
    /// When both are set up, texts go through RingCentral.
    pub ringcentral: bool,
}
