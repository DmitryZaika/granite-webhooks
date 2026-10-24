//! Who carries a company's calls. Separate from `SmsProvider`: a company
//! may call through one provider and text through another.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// A telephony provider of the call routes: `cloudtalk`, `ringcentral`, or
/// `telnyx` (Granite Phone, whose calls are in our own call log).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum CallProvider {
    Cloudtalk,
    Ringcentral,
    Telnyx,
}

impl CallProvider {
    /// As the paths spell it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cloudtalk => "cloudtalk",
            Self::Ringcentral => "ringcentral",
            Self::Telnyx => "telnyx",
        }
    }
}
