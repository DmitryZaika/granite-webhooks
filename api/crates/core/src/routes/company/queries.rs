use super::schemas::CompanyFeatures;
use crate::sms_provider::SmsProvider;
use sqlx::{AssertSqlSafe, FromRow, MySqlPool};

#[derive(Debug, FromRow)]
struct FeatureRow {
    quickbooks_enabled: bool,
    quickbooks_connected: bool,
    onboarding: bool,
    leaderboard: bool,
    e_signature: bool,
    domain: Option<String>,
    cloudtalk: bool,
    ringcentral: bool,
    telnyx: bool,
    telnyx_sms: bool,
    auto_sale: bool,
    auto_unscheduled_jobs: bool,
}

/// `company c` columns plus the platform-wide rollout defaults
/// (`global_feature_flags`): a company's own NULL switch falls back to them.
/// Remix: `ROLLOUT_FEATURE_SELECT_SQL` / `ROLLOUT_FEATURE_JOIN_SQL`.
pub const ROLLOUT_JOINS: &str = "
    LEFT JOIN global_feature_flags gf_onboarding ON gf_onboarding.feature_key = 'onboarding'
    LEFT JOIN global_feature_flags gf_leaderboard ON gf_leaderboard.feature_key = 'leaderboard'";
pub const ONBOARDING_ON: &str =
    "(COALESCE(c.onboarding_enabled, gf_onboarding.enabled_for_all, 0) <> 0)";
pub const LEADERBOARD_ON: &str =
    "(COALESCE(c.leaderboard_enabled, gf_leaderboard.enabled_for_all, 0) <> 0)";

fn filled(column: &str) -> String {
    format!("(COALESCE(TRIM({column}), '') <> '')")
}

/// `companySignatureEnabled`: the company uses eSignatures, i.e. at least one
/// of its users may send for signature. Access is given per user
/// (`users.is_signature_available`, Superuser › Feature access); the old
/// company switch (`company.signature_enabled`) is no longer read.
pub const SIGNATURE_ON: &str = "EXISTS (SELECT 1 FROM users su
                                 WHERE su.company_id = c.id AND su.is_deleted = 0
                                   AND COALESCE(su.is_signature_available, 0) <> 0)";

/// Remix: `loadCompanyFeatures`, `isCompanyQboConnected`,
/// `companyHasInboundEmail`, `companyHasCloudTalk`, `companyHasRingCentral`.
///
/// Remix also decrypted the saved OAuth tokens and counted a connection whose
/// tokens fail to decrypt as missing; the API has no key and only checks they
/// are saved (Known differences in the frontend's runbook).
pub async fn company_features(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<CompanyFeatures, sqlx::Error> {
    let sql = format!(
        "SELECT c.quickbooks_enabled <> 0 AS quickbooks_enabled,
                ({qbo_realm} AND c.qbo_access_token IS NOT NULL
                 AND c.qbo_refresh_token IS NOT NULL
                 AND c.qbo_access_expires IS NOT NULL
                 AND c.qbo_refresh_expires IS NOT NULL) AS quickbooks_connected,
                {ONBOARDING_ON} AS onboarding,
                {LEADERBOARD_ON} AS leaderboard,
                {SIGNATURE_ON} AS e_signature,
                c.domain,
                ({ct_key} AND {ct_secret}) AS cloudtalk,
                ((c.ringcentral_access_token IS NOT NULL
                  AND c.ringcentral_refresh_token IS NOT NULL
                  AND c.ringcentral_access_expires IS NOT NULL
                  AND c.ringcentral_refresh_expires > UNIX_TIMESTAMP())
                 OR ({rc_id} AND {rc_secret} AND {rc_jwt})) AS ringcentral,
                EXISTS (SELECT 1 FROM telnyx_company_config t
                         WHERE t.company_id = c.id AND t.enabled <> 0) AS telnyx,
                EXISTS (SELECT 1 FROM telnyx_company_config t
                         WHERE t.company_id = c.id AND t.enabled <> 0
                           AND COALESCE(TRIM(t.messaging_profile_id), '') <> '') AS telnyx_sms,
                c.auto_sale <> 0 AS auto_sale,
                c.auto_unscheduled_jobs <> 0 AS auto_unscheduled_jobs
           FROM company c
           {ROLLOUT_JOINS}
          WHERE c.id = ?",
        qbo_realm = filled("c.qbo_realm_id"),
        ct_key = filled("c.cloudtalk_access_key"),
        ct_secret = filled("c.cloudtalk_access_secret"),
        rc_id = filled("c.ringcentral_client_id"),
        rc_secret = filled("c.ringcentral_client_secret"),
        rc_jwt = filled("c.ringcentral_jwt"),
    );
    let row = sqlx::query_as::<_, FeatureRow>(AssertSqlSafe(sql))
        .bind(company_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map_or_else(
        // Remix DEFAULT_COMPANY_FEATURES: QuickBooks is on unless switched off.
        || CompanyFeatures {
            quickbooks_enabled: true,
            quickbooks_connected: false,
            onboarding: false,
            leaderboard: false,
            e_signature: false,
            inbound_email: false,
            cloudtalk: false,
            ringcentral: false,
            telnyx: false,
            telnyx_sms: false,
            auto_sale: false,
            auto_unscheduled_jobs: false,
        },
        |row| CompanyFeatures {
            quickbooks_enabled: row.quickbooks_enabled,
            quickbooks_connected: row.quickbooks_connected,
            onboarding: row.onboarding,
            leaderboard: row.leaderboard,
            e_signature: row.e_signature,
            inbound_email: mail_domain(row.domain.as_deref()).is_some(),
            cloudtalk: row.cloudtalk,
            ringcentral: row.ringcentral,
            telnyx: row.telnyx,
            telnyx_sms: row.telnyx_sms,
            auto_sale: row.auto_sale,
            auto_unscheduled_jobs: row.auto_unscheduled_jobs,
        },
    ))
}

impl CompanyFeatures {
    /// The provider texts go through: RingCentral when both are set up.
    /// Remix: `getCompanySmsProvider`.
    pub const fn sms_provider(&self) -> Option<SmsProvider> {
        if self.ringcentral {
            Some(SmsProvider::Ringcentral)
        } else if self.cloudtalk {
            Some(SmsProvider::Cloudtalk)
        } else {
            None
        }
    }
}

/// The bare mail domain in `company.domain` (which admins type freely:
/// `https://www.acme.com/`, `sales@acme.com`), or None when it is not one.
/// Remix: `extractCompanyDomain`.
pub fn mail_domain(input: Option<&str>) -> Option<String> {
    let mut value = input?.trim().to_lowercase();
    if let Some(at) = value.rfind('@') {
        value = value[at + 1..].to_string();
    }
    let value = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .unwrap_or(&value);
    let value = value.split(['/', '?', '#']).next().unwrap_or_default();
    let value = match value.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => value,
    };
    let value = value.strip_prefix("www.").unwrap_or(value);
    let labels: Vec<&str> = value.split('.').collect();
    let valid_label = |label: &&str| {
        let bytes = label.as_bytes();
        !bytes.is_empty()
            && bytes
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
            && bytes[0] != b'-'
            && bytes[bytes.len() - 1] != b'-'
    };
    (labels.len() >= 2 && labels.iter().all(valid_label)).then(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::mail_domain;

    #[test]
    fn mail_domain_matches_remix() {
        for (input, expected) in [
            (Some("acme.com"), Some("acme.com")),
            (
                Some("  HTTPS://www.Acme.com/contact?x#y "),
                Some("acme.com"),
            ),
            (Some("sales@acme.co.uk"), Some("acme.co.uk")),
            (Some("acme.com:8080"), Some("acme.com")),
            (Some("my-shop.example"), Some("my-shop.example")),
            (Some("localhost"), None),
            (Some("-bad.com"), None),
            (Some("bad_.com"), None),
            (Some(""), None),
            (None, None),
        ] {
            assert_eq!(mail_domain(input).as_deref(), expected, "{input:?}");
        }
    }
}
