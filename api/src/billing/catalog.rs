//! The self-serve price list: Stripe prices found by lookup key, and the
//! quote a buyer sees (base, extra users, add-ons, free period, onboarding
//! fee). Every total the payments app shows comes from here.
//!
//! Prices (03-stripe.md section 4.1): `granite_base_monthly_v1` (up to
//! [`INCLUDED_USERS`] users), `granite_extra_user_monthly_v1` (each billable
//! user above them) and one `addon_<key>_monthly_v1` per add-on. Loaded with
//! `GET /v1/prices?lookup_keys[]=...` and cached 10 minutes per Stripe account.

use super::stripe::{Stripe, StripeError};
use crate::entitlements::AddonKey;
use chrono::{DateTime, Duration, Months, NaiveDate, NaiveDateTime, SubsecRound, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration as StdDuration, Instant};
use utoipa::ToSchema;

pub const BASE_LOOKUP_KEY: &str = "granite_base_monthly_v1";
pub const EXTRA_USER_LOOKUP_KEY: &str = "granite_extra_user_monthly_v1";
/// Users the base price covers.
pub const INCLUDED_USERS: i64 = 10;
/// Months between signup and the first charge.
pub const FREE_MONTHS: u32 = 3;
/// Days after signup to complete onboarding before the fee.
pub const ONBOARDING_DEADLINE_DAYS: i64 = 30;
/// Bounds of the user estimate in step 1.
pub const MIN_USERS: i64 = 1;
pub const MAX_USERS: i64 = 500;
pub const DEFAULT_USERS: i64 = 5;
/// The calendar buyers' dates are counted in.
pub const SIGNUP_TZ: chrono_tz::Tz = chrono_tz::America::Indiana::Indianapolis;

/// How long a loaded catalog is reused.
const CACHE_TTL: StdDuration = StdDuration::from_secs(600);

/// Loaded catalogs by Stripe base URL (one account each).
static CACHE: LazyLock<Mutex<HashMap<String, (Instant, Catalog)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// One Stripe price of the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogPrice {
    pub price_id: String,
    pub lookup_key: String,
    pub unit_cents: i64,
}

/// What a subscription item's price is, by its lookup key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceKind {
    Base,
    ExtraUser,
    Addon(AddonKey),
}

impl PriceKind {
    pub fn from_lookup_key(key: &str) -> Option<Self> {
        match key {
            BASE_LOOKUP_KEY => Some(Self::Base),
            EXTRA_USER_LOOKUP_KEY => Some(Self::ExtraUser),
            other => AddonKey::from_lookup_key(other).map(Self::Addon),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error(transparent)]
    Stripe(#[from] StripeError),
    /// A lookup key has no active monthly USD per-unit price.
    #[error("Stripe price {0} is missing or not a monthly USD per-unit price")]
    Invalid(String),
}

impl CatalogError {
    /// Safe to log: Stripe's error code, never its message.
    pub fn reason(&self) -> String {
        match self {
            Self::Stripe(err) => format!("stripe {}", err.code()),
            Self::Invalid(_) => self.to_string(),
        }
    }
}

/// The live prices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catalog {
    pub base: CatalogPrice,
    pub extra_user: CatalogPrice,
    /// Every add-on, in [`AddonKey::ALL`] order.
    pub addons: Vec<(AddonKey, CatalogPrice)>,
}

/// What the buyer picks in step 1. `users` is an estimate: billing counts
/// active users.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PlanChoice {
    /// Estimated active users (1 to 500); only previews the price.
    pub users: i64,
    /// Chosen add-ons.
    pub addons: Vec<AddonKey>,
}

impl PlanChoice {
    /// Users clamped to [`MIN_USERS`]..=[`MAX_USERS`], add-ons deduplicated
    /// in [`AddonKey::ALL`] order.
    pub fn normalized(users: i64, addons: &[AddonKey]) -> Self {
        Self {
            users: users.clamp(MIN_USERS, MAX_USERS),
            addons: AddonKey::ALL
                .into_iter()
                .filter(|key| addons.contains(key))
                .collect(),
        }
    }
}

/// One line of a quote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct QuoteLine {
    /// `base`, `extra_users` or `addon`.
    pub kind: QuoteLineKind,
    /// The add-on of an `addon` line, else null.
    pub key: Option<AddonKey>,
    /// Display label, e.g. "Base plan (up to 10 users)", "2 extra users", "Quotes".
    pub label: String,
    /// How many: 1 for the base and add-ons, the extra users otherwise.
    pub quantity: i64,
    /// Price of one, in cents.
    pub unit_cents: i64,
    /// `quantity` x `unit_cents`.
    pub amount_cents: i64,
}

/// What a quote line charges for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuoteLineKind {
    Base,
    ExtraUsers,
    Addon,
}

/// The price of a plan choice. Money in cents, before sales tax.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct Quote {
    /// The user estimate the quote was made for.
    pub users: i64,
    /// Users the base price covers (10).
    pub included_users: i64,
    /// Users above the included ones.
    pub extra_users: i64,
    /// Base, then extra users (when any), then each add-on.
    pub lines: Vec<QuoteLine>,
    /// Base + add-ons, without extra users: the fixed part of each charge.
    pub base_and_addons_cents: i64,
    /// Estimated monthly total with the extra users.
    pub monthly_cents: i64,
    /// Always 0: the free period.
    pub due_today_cents: i64,
    /// First charge day (`YYYY-MM-DD`): signup day + 3 calendar months.
    pub first_charge_on: NaiveDate,
    /// Onboarding fee: 2 x `base_and_addons_cents`, waived when onboarding is
    /// completed by `deadline_on`.
    pub fee_cents: i64,
    /// Onboarding deadline (`YYYY-MM-DD`): signup day + 30 days.
    pub deadline_on: NaiveDate,
}

impl Catalog {
    /// The prices, from a 10-minute cache per Stripe account.
    pub async fn load(stripe: &Stripe) -> Result<Self, CatalogError> {
        if !stripe.is_enabled() {
            return Err(StripeError::NotConfigured.into());
        }
        let base = stripe.base_url().to_string();
        if let Some((loaded, catalog)) = CACHE
            .lock()
            .ok()
            .and_then(|cache| cache.get(&base).cloned())
            && loaded.elapsed() < CACHE_TTL
        {
            return Ok(catalog);
        }
        let keys: Vec<&str> = [BASE_LOOKUP_KEY, EXTRA_USER_LOOKUP_KEY]
            .into_iter()
            .chain(AddonKey::ALL.map(AddonKey::lookup_key))
            .collect();
        let prices = stripe.prices_by_lookup_keys(&keys).await?;
        let catalog = Self::from_prices(&prices)?;
        if let Ok(mut cache) = CACHE.lock() {
            cache.insert(base, (Instant::now(), catalog.clone()));
        }
        Ok(catalog)
    }

    fn from_prices(prices: &[Value]) -> Result<Self, CatalogError> {
        let find = |key: &str| {
            prices
                .iter()
                .find(|price| price["lookup_key"] == key)
                .and_then(|price| valid_price(price, key))
                .ok_or_else(|| CatalogError::Invalid(key.to_string()))
        };
        let addons = AddonKey::ALL
            .into_iter()
            .map(|key| Ok((key, find(key.lookup_key())?)))
            .collect::<Result<Vec<_>, CatalogError>>()?;
        Ok(Self {
            base: find(BASE_LOOKUP_KEY)?,
            extra_user: find(EXTRA_USER_LOOKUP_KEY)?,
            addons,
        })
    }

    pub fn addon(&self, key: AddonKey) -> &CatalogPrice {
        self.addons
            .iter()
            .find(|(addon, _)| *addon == key)
            .map(|(_, price)| price)
            .expect("the catalog has every add-on")
    }

    /// The quote for `plan` signed up on `signup_day` (see [`signup_day`]).
    pub fn quote(&self, plan: &PlanChoice, signup_day: NaiveDate) -> Quote {
        let plan = PlanChoice::normalized(plan.users, &plan.addons);
        let extra = extra_users(plan.users);
        let mut lines = vec![QuoteLine {
            kind: QuoteLineKind::Base,
            key: None,
            label: format!("Base plan (up to {INCLUDED_USERS} users)"),
            quantity: 1,
            unit_cents: self.base.unit_cents,
            amount_cents: self.base.unit_cents,
        }];
        if extra > 0 {
            lines.push(QuoteLine {
                kind: QuoteLineKind::ExtraUsers,
                key: None,
                label: format!("{extra} extra user{}", if extra == 1 { "" } else { "s" }),
                quantity: extra,
                unit_cents: self.extra_user.unit_cents,
                amount_cents: extra * self.extra_user.unit_cents,
            });
        }
        let mut base_and_addons_cents = self.base.unit_cents;
        for key in &plan.addons {
            let price = self.addon(*key);
            base_and_addons_cents += price.unit_cents;
            lines.push(QuoteLine {
                kind: QuoteLineKind::Addon,
                key: Some(*key),
                label: key.label().to_string(),
                quantity: 1,
                unit_cents: price.unit_cents,
                amount_cents: price.unit_cents,
            });
        }
        Quote {
            users: plan.users,
            included_users: INCLUDED_USERS,
            extra_users: extra,
            lines,
            base_and_addons_cents,
            monthly_cents: base_and_addons_cents + extra * self.extra_user.unit_cents,
            due_today_cents: 0,
            first_charge_on: first_charge_on(signup_day),
            fee_cents: 2 * base_and_addons_cents,
            deadline_on: deadline_on(signup_day),
        }
    }

    /// The subscription's prices: the base first, then the chosen add-ons in
    /// [`AddonKey::ALL`] order. Extra users are added later by seat pushes.
    pub fn subscription_price_ids(&self, addons: &[AddonKey]) -> Vec<String> {
        std::iter::once(self.base.price_id.clone())
            .chain(
                AddonKey::ALL
                    .into_iter()
                    .filter(|key| addons.contains(key))
                    .map(|key| self.addon(key).price_id.clone()),
            )
            .collect()
    }
}

/// The buyer's calendar day at `now` ([`SIGNUP_TZ`]).
pub fn signup_day(now: DateTime<Utc>) -> NaiveDate {
    now.with_timezone(&SIGNUP_TZ).date_naive()
}

/// Signup day + [`FREE_MONTHS`] calendar months, clamped to the month's last
/// day (Nov 30 -> Feb 28).
pub fn first_charge_on(signup_day: NaiveDate) -> NaiveDate {
    signup_day
        .checked_add_months(Months::new(FREE_MONTHS))
        .unwrap_or(signup_day)
}

/// Signup day + [`ONBOARDING_DEADLINE_DAYS`].
pub fn deadline_on(signup_day: NaiveDate) -> NaiveDate {
    signup_day + Duration::days(ONBOARDING_DEADLINE_DAYS)
}

/// When the free period ends (UTC): the signup's local wall time on
/// [`first_charge_on`], so the charge lands on the day the buyer was shown.
pub fn trial_end(now: DateTime<Utc>) -> NaiveDateTime {
    let local = now.with_timezone(&SIGNUP_TZ);
    let wall = first_charge_on(local.date_naive()).and_time(local.time());
    let resolved = SIGNUP_TZ.from_local_datetime(&wall).earliest().or_else(|| {
        SIGNUP_TZ
            .from_local_datetime(&(wall + Duration::hours(1)))
            .earliest()
    });
    resolved
        .map_or_else(
            || (now + Duration::days(92)).naive_utc(),
            |date| date.naive_utc(),
        )
        .trunc_subsecs(0)
}

/// When the onboarding deadline passes (UTC): `now` + 30 days.
pub fn onboarding_deadline(now: DateTime<Utc>) -> NaiveDateTime {
    (now + Duration::days(ONBOARDING_DEADLINE_DAYS))
        .naive_utc()
        .trunc_subsecs(0)
}

/// Units of the extra-user item for `users` users.
const fn extra_users(users: i64) -> i64 {
    super::seats::extra_users(users)
}

/// The price when it is an active monthly USD per-unit price with an amount.
fn valid_price(price: &Value, lookup_key: &str) -> Option<CatalogPrice> {
    let recurring = &price["recurring"];
    let valid = price["active"] == true
        && price["currency"] == "usd"
        && price["billing_scheme"] == "per_unit"
        && recurring["interval"] == "month"
        && recurring["interval_count"] == 1;
    if !valid {
        return None;
    }
    Some(CatalogPrice {
        price_id: price["id"].as_str()?.to_string(),
        lookup_key: lookup_key.to_string(),
        unit_cents: price["unit_amount"].as_i64()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn price(id: &str, key: &str, cents: i64) -> CatalogPrice {
        CatalogPrice {
            price_id: id.into(),
            lookup_key: key.into(),
            unit_cents: cents,
        }
    }

    fn catalog() -> Catalog {
        Catalog {
            base: price("price_base", BASE_LOOKUP_KEY, 30_000),
            extra_user: price("price_extra", EXTRA_USER_LOOKUP_KEY, 3_000),
            addons: vec![
                (AddonKey::Signatures, price("price_sig", "s", 1_500)),
                (AddonKey::Quotes, price("price_quotes", "q", 7_500)),
                (AddonKey::Quickbooks, price("price_qb", "b", 3_000)),
            ],
        }
    }

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("date")
    }

    fn utc(text: &str) -> DateTime<Utc> {
        text.parse().expect("time")
    }

    #[test]
    fn quotes_twelve_users_with_quotes() {
        let plan = PlanChoice::normalized(12, &[AddonKey::Quotes]);
        let quote = catalog().quote(&plan, day(2026, 10, 23));
        assert_eq!(quote.extra_users, 2);
        assert_eq!(quote.base_and_addons_cents, 37_500);
        assert_eq!(quote.monthly_cents, 43_500);
        assert_eq!(quote.fee_cents, 75_000);
        assert_eq!(quote.due_today_cents, 0);
        assert_eq!(quote.first_charge_on, day(2027, 1, 23));
        assert_eq!(quote.deadline_on, day(2026, 11, 22));
        let lines: Vec<(&str, i64, i64)> = quote
            .lines
            .iter()
            .map(|line| (line.label.as_str(), line.quantity, line.amount_cents))
            .collect();
        assert_eq!(
            lines,
            vec![
                ("Base plan (up to 10 users)", 1, 30_000),
                ("2 extra users", 2, 6_000),
                ("Quotes", 1, 7_500),
            ]
        );
    }

    #[test]
    fn quotes_one_user_without_addons() {
        let quote = catalog().quote(&PlanChoice::normalized(1, &[]), day(2026, 10, 23));
        assert_eq!(quote.extra_users, 0);
        assert_eq!(quote.lines.len(), 1);
        assert_eq!(quote.monthly_cents, 30_000);
        assert_eq!(quote.fee_cents, 60_000);
        let one_extra = catalog().quote(&PlanChoice::normalized(11, &[]), day(2026, 10, 23));
        assert_eq!(one_extra.lines[1].label, "1 extra user");
    }

    #[test]
    fn normalizes_users_and_addons() {
        let plan = PlanChoice::normalized(
            9_999,
            &[
                AddonKey::Quickbooks,
                AddonKey::Signatures,
                AddonKey::Quickbooks,
            ],
        );
        assert_eq!(plan.users, 500);
        assert_eq!(
            plan.addons,
            vec![AddonKey::Signatures, AddonKey::Quickbooks]
        );
        assert_eq!(PlanChoice::normalized(-4, &[]).users, 1);
        let quote = catalog().quote(
            &PlanChoice {
                users: 0,
                addons: vec![AddonKey::Quotes, AddonKey::Quotes],
            },
            day(2026, 10, 23),
        );
        assert_eq!((quote.users, quote.lines.len()), (1, 2));
    }

    #[test]
    fn subscription_prices_follow_all_order() {
        assert_eq!(
            catalog().subscription_price_ids(&[AddonKey::Quickbooks, AddonKey::Signatures]),
            vec!["price_base", "price_sig", "price_qb"]
        );
    }

    #[test]
    fn month_ends_are_clamped() {
        assert_eq!(first_charge_on(day(2026, 11, 30)), day(2027, 2, 28));
        assert_eq!(first_charge_on(day(2027, 11, 30)), day(2028, 2, 29));
        assert_eq!(first_charge_on(day(2026, 1, 31)), day(2026, 4, 30));
        assert_eq!(deadline_on(day(2026, 12, 15)), day(2027, 1, 14));
    }

    #[test]
    fn trial_end_keeps_the_local_wall_time() {
        let end = trial_end(utc("2026-10-23T19:00:00Z"));
        assert_eq!(end.to_string(), "2027-01-23 20:00:00");
        let sub_second = trial_end(utc("2026-10-23T19:00:00.750Z"));
        assert_eq!(sub_second.to_string(), "2027-01-23 20:00:00");
    }

    #[test]
    fn signup_day_is_the_local_day() {
        assert_eq!(signup_day(utc("2026-10-24T02:30:00Z")), day(2026, 10, 23));
        assert_eq!(signup_day(utc("2026-10-24T05:30:00Z")), day(2026, 10, 24));
    }

    #[test]
    fn onboarding_deadline_is_thirty_days_on() {
        assert_eq!(
            onboarding_deadline(utc("2026-10-23T19:00:00.500Z")).to_string(),
            "2026-11-22 19:00:00"
        );
    }

    #[test]
    fn rejects_prices_that_are_not_monthly_usd_per_unit() {
        let good = serde_json::json!({
            "id": "p", "active": true, "currency": "usd", "billing_scheme": "per_unit",
            "unit_amount": 100, "recurring": { "interval": "month", "interval_count": 1 },
        });
        assert!(valid_price(&good, "k").is_some());
        for (field, value) in [
            ("active", serde_json::json!(false)),
            ("currency", serde_json::json!("eur")),
            ("billing_scheme", serde_json::json!("tiered")),
            ("unit_amount", serde_json::Value::Null),
            (
                "recurring",
                serde_json::json!({ "interval": "year", "interval_count": 1 }),
            ),
            (
                "recurring",
                serde_json::json!({ "interval": "month", "interval_count": 3 }),
            ),
        ] {
            let mut bad = good.clone();
            bad[field] = value;
            assert!(valid_price(&bad, "k").is_none(), "{field}");
        }
    }
}
