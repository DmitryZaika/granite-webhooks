//! A rep's texting line saved, from My Account or the Users page: the
//! `RingCentral` SMS webhook covers their extension, and their texts follow
//! them to the provider they text through now (`crate::routes::sms::moves`).

use crate::routes::sms::moves::{self, Kind};
use crate::routes::sms::provider::SmsProvider;
use crate::state::AppState;
use common::telephony::ringcentral;
use lambda_http::tracing;

/// A user's texting line, before a save or as saved. Numbers are the last
/// ten digits.
#[derive(Debug, Clone, Default)]
pub struct Line {
    pub cloudtalk_agent: Option<String>,
    pub cloudtalk_phone: Option<String>,
    pub ringcentral_extension: Option<String>,
    pub ringcentral_phone: Option<String>,
}

/// After a save turned `before` into `after`. `matched`: `RingCentral` found
/// the saved number, so the subscription is renewed with the current
/// extensions (it never expires, so a rep added since is covered only by a
/// renewal). The ids the save replaced are remembered, and the rep's texts
/// copied to the provider they text through now. Failures are logged; the
/// save stands.
pub async fn follow(
    state: &AppState,
    company_id: i32,
    user_id: i32,
    before: &Line,
    after: &Line,
    matched: bool,
) {
    let pool = &state.pool;
    if let Some(extension) = after.ringcentral_extension.as_deref()
        && (matched || before.ringcentral_extension.as_deref() != Some(extension))
        && let Err(error) =
            ringcentral::ensure_sms_subscription(pool, &state.telephony_http, company_id).await
    {
        tracing::error!(company_id, ?error, "RingCentral SMS subscription failed");
    }
    let replaced = [
        (
            SmsProvider::Cloudtalk,
            Kind::Agent,
            before.cloudtalk_agent.as_deref(),
        ),
        (
            SmsProvider::Cloudtalk,
            Kind::Line,
            before.cloudtalk_phone.as_deref(),
        ),
        (
            SmsProvider::Ringcentral,
            Kind::Agent,
            before.ringcentral_extension.as_deref(),
        ),
        (
            SmsProvider::Ringcentral,
            Kind::Line,
            before.ringcentral_phone.as_deref(),
        ),
    ];
    let carried = match moves::remember(pool, company_id, user_id, &replaced).await {
        Ok(()) => moves::carry(pool, company_id, user_id).await,
        Err(error) => Err(error),
    };
    if let Err(error) = carried {
        tracing::error!(company_id, user_id, %error, "copying texts to the new provider failed");
    }
}
