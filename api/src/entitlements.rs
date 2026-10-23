//! Paid add-ons.
//!
//! A legacy company (`company.plan_source = 'legacy'`, every
//! company before self-serve) has them all, as today; a subscription company
//! only those with an active `company_addons` row (bought, or granted free by
//! a superuser). Existing switches (`quickbooks_enabled`, `signature_enabled`)
//! still apply on top.
//!
//! Session routes check `user.plan.require(key)`: the session query already
//! loads the plan, so the check costs no query. Routes without a session use
//! [`require_addon`]; SQL that already reads the company row uses
//! [`entitled_sql`].

use crate::error::{ApiError, ApiResult};
use serde::Serialize;
use sqlx::{FromRow, MySqlPool};
use utoipa::ToSchema;

/// A paid add-on: `signatures`, `quotes` or `quickbooks`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AddonKey {
    /// eSignatures and the users' email signature block.
    Signatures,
    /// Quotes, their settings, the online quote form and commissions.
    Quotes,
    /// QuickBooks invoices and the invoice queue.
    Quickbooks,
}

impl AddonKey {
    pub const ALL: [Self; 3] = [Self::Signatures, Self::Quotes, Self::Quickbooks];

    /// `company_addons.addon_key` and the API's name for it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Signatures => "signatures",
            Self::Quotes => "quotes",
            Self::Quickbooks => "quickbooks",
        }
    }

    /// The name people see.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Signatures => "Signatures",
            Self::Quotes => "Quotes",
            Self::Quickbooks => "QuickBooks",
        }
    }

    /// The Stripe price lookup key of its monthly price.
    pub const fn lookup_key(self) -> &'static str {
        match self {
            Self::Signatures => "addon_signatures_monthly_v1",
            Self::Quotes => "addon_quotes_monthly_v1",
            Self::Quickbooks => "addon_quickbooks_monthly_v1",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|key| key.as_str() == value)
    }

    pub fn from_lookup_key(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|key| key.lookup_key() == value)
    }
}

/// A `company_addons ca` row in force now.
pub const ACTIVE_ADDON: &str =
    "ca.started_at <= UTC_TIMESTAMP() AND (ca.ended_at IS NULL OR ca.ended_at > UTC_TIMESTAMP())";

/// SQL condition: the company row aliased `company` may use `key`. A missing
/// (LEFT JOINed) company counts as legacy.
pub fn entitled_sql(company: &str, key: AddonKey) -> String {
    format!(
        "(COALESCE({company}.plan_source, 'legacy') <> 'subscription'
          OR EXISTS (SELECT 1 FROM company_addons ca
                      WHERE ca.company_id = {company}.id AND ca.addon_key = '{key}'
                        AND {ACTIVE_ADDON}))",
        key = key.as_str()
    )
}

/// The `plan_subscription` and `plan_addons` columns [`Entitlements`] reads,
/// for the company row aliased `company`.
pub fn plan_columns(company: &str) -> String {
    format!(
        "COALESCE({company}.plan_source = 'subscription', 0) AS plan_subscription,
         (SELECT GROUP_CONCAT(ca.addon_key ORDER BY ca.addon_key) FROM company_addons ca
           WHERE ca.company_id = {company}.id AND {ACTIVE_ADDON}) AS plan_addons"
    )
}

/// What a company's plan lets it use.
#[derive(Debug, Clone, Default, PartialEq, Eq, FromRow)]
pub struct Entitlements {
    /// The company is on a subscription plan; otherwise it is legacy.
    #[sqlx(rename = "plan_subscription")]
    pub subscription: bool,
    /// Its active add-on keys, comma separated.
    #[sqlx(rename = "plan_addons")]
    addons: Option<String>,
}

impl Entitlements {
    /// A subscription plan with exactly these add-ons.
    pub fn subscription(keys: &[AddonKey]) -> Self {
        let addons: Vec<&str> = keys.iter().map(|key| key.as_str()).collect();
        Self {
            subscription: true,
            addons: Some(addons.join(",")),
        }
    }

    pub fn has(&self, key: AddonKey) -> bool {
        !self.subscription
            || self
                .addons
                .as_deref()
                .is_some_and(|keys| keys.split(',').any(|k| k == key.as_str()))
    }

    /// 403 `addon_required` unless the plan has `key`.
    pub fn require(&self, key: AddonKey) -> ApiResult<()> {
        if self.has(key) {
            Ok(())
        } else {
            Err(ApiError::AddonRequired(key))
        }
    }

    /// The add-ons the company may use (all of them on a legacy plan).
    pub fn entitled(&self) -> Vec<AddonKey> {
        AddonKey::ALL
            .into_iter()
            .filter(|key| self.has(*key))
            .collect()
    }

    /// `legacy` or `subscription`.
    pub const fn source(&self) -> &'static str {
        if self.subscription {
            "subscription"
        } else {
            "legacy"
        }
    }
}

/// The plan of `company_id` (legacy when there is no such company).
pub async fn company_entitlements(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<Entitlements, sqlx::Error> {
    let row: Option<Entitlements> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {} FROM company c WHERE c.id = ?",
        plan_columns("c")
    )))
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.unwrap_or_default())
}

/// 403 `addon_required` unless `company_id`'s plan has `key`. For routes
/// without a session; session routes check `user.plan`.
pub async fn require_addon(pool: &MySqlPool, company_id: i32, key: AddonKey) -> ApiResult<()> {
    company_entitlements(pool, company_id).await?.require(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_plans_have_every_addon() {
        let legacy = Entitlements::default();
        for key in AddonKey::ALL {
            assert!(legacy.has(key));
            assert!(legacy.require(key).is_ok());
        }
        assert_eq!(legacy.entitled(), AddonKey::ALL.to_vec());
        assert_eq!(legacy.source(), "legacy");
    }

    #[test]
    fn subscription_plans_have_only_their_addons() {
        let plan = Entitlements::subscription(&[AddonKey::Quotes]);
        assert!(plan.has(AddonKey::Quotes));
        assert!(!plan.has(AddonKey::Signatures));
        assert!(matches!(
            plan.require(AddonKey::Quickbooks),
            Err(ApiError::AddonRequired(AddonKey::Quickbooks))
        ));
        assert_eq!(plan.entitled(), vec![AddonKey::Quotes]);
        assert!(Entitlements::subscription(&[]).entitled().is_empty());
        assert_eq!(plan.source(), "subscription");
    }

    #[test]
    fn keys_round_trip() {
        for key in AddonKey::ALL {
            assert_eq!(AddonKey::parse(key.as_str()), Some(key));
            assert_eq!(AddonKey::from_lookup_key(key.lookup_key()), Some(key));
        }
        assert_eq!(AddonKey::parse("quote"), None);
    }
}
