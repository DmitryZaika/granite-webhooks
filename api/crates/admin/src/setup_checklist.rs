//! A company's setup checklist for super admins.
//!
//! Each item is done automatically (from the database, DNS or SES) or ticked
//! by hand (`company_setup_checklist.manual_overrides`). Port of the CRM's
//! `companySetupChecklist.server.ts`; its stones-website probe only ever ran
//! from an unused refresh, so that item reads the stored `auto_cache`.

use crate::deliverability::Dns;
use crate::openapi::ToSchema;
use crate::state::AppState;
use serde::Serialize;
use serde_json::{Map, Value};
use sqlx::MySqlPool;
use std::collections::{HashMap, HashSet};

/// Every item, in the order shown.
pub const ITEM_KEYS: [&str; 15] = [
    "subdomain_infra",
    "domain",
    "users.manager",
    "users.installer",
    "users.sales_rep",
    "users.marketing",
    "users.shop_worker",
    "inbound_email",
    "stones_website",
    "onboarding.manager",
    "onboarding.inventory_manager",
    "onboarding.sales_reps",
    "quickbooks",
    "cloudtalk",
    "ringcentral",
];

fn label(key: &str) -> &'static str {
    match key {
        "subdomain_infra" => "Add subdomain to Caddy and AWS",
        "domain" => "Add domain to the company",
        "users.manager" => "Manager",
        "users.installer" => "Installer",
        "users.sales_rep" => "Sales reps",
        "users.marketing" => "Marketing",
        "users.shop_worker" => "Shop worker",
        "inbound_email" => "Set up inbound emails with AWS (SES verified)",
        "stones_website" => "Place stones on the company website",
        "onboarding.manager" => "Onboarding: Manager",
        "onboarding.inventory_manager" => "Onboarding: Inventory manager",
        "onboarding.sales_reps" => "Onboarding: Sales reps",
        "quickbooks" => "QuickBooks connected",
        "cloudtalk" => "CloudTalk connected",
        _ => "RingCentral connected",
    }
}

const USER_POSITIONS: [(&str, i32); 5] = [
    ("users.manager", 5),
    ("users.installer", 6),
    ("users.sales_rep", 1),
    ("users.marketing", 7),
    ("users.shop_worker", 4),
];

/// One item.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Item {
    /// Item key (e.g. `users.manager`); `PUT .../setup-checklist/items/{key}` ticks it.
    pub key: String,
    /// What to do.
    pub label: String,
    /// Done automatically or by hand.
    pub done: bool,
    /// Done automatically (from the database, DNS or SES).
    pub auto: bool,
    /// Ticked by hand.
    pub manual: bool,
    /// `auto`, `manual` or `none`.
    pub source: String,
    /// A hint (the domain, the expected DNS name), or null.
    pub detail: Option<String>,
}

/// A company's checklist.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Checklist {
    /// The company.
    pub company_id: i32,
    /// The items, in order.
    pub items: Vec<Item>,
    /// Items done.
    pub done_count: usize,
    /// Items in all.
    pub total_count: usize,
}

/// What the checklist reads of a company.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CompanyFacts {
    pub id: i32,
    pub subdomain: Option<String>,
    pub domain: Option<String>,
    pub cloudtalk: bool,
    pub ringcentral_jwt_set: bool,
    pub qbo_realm_id: Option<String>,
    pub quickbooks_enabled: bool,
    /// A self-serve company: it lives on the apex, a subdomain is not expected.
    pub self_serve: bool,
}

/// The select list for [`CompanyFacts`] over `company c`.
pub const FACTS_SQL: &str = "c.id, c.subdomain, c.domain,
    (COALESCE(c.cloudtalk_access_key, '') <> '' AND COALESCE(c.cloudtalk_access_secret, '') <> '') AS cloudtalk,
    (COALESCE(TRIM(c.ringcentral_client_id), '') <> '' AND COALESCE(TRIM(c.ringcentral_client_secret), '') <> ''
     AND COALESCE(TRIM(c.ringcentral_jwt), '') <> '') AS ringcentral_jwt_set,
    c.qbo_realm_id, COALESCE(c.quickbooks_enabled, 1) <> 0 AS quickbooks_enabled,
    c.plan_source = 'subscription' AS self_serve";

/// The domain of a domain field or email address (the checklist's lenient reading).
fn domain_host(value: Option<&str>) -> Option<String> {
    let trimmed = value?.trim().to_lowercase();
    if trimmed.is_empty() {
        return None;
    }
    if let Some((_, host)) = trimmed.split_once('@') {
        let host = host.trim();
        return (!host.is_empty()).then(|| host.to_string());
    }
    let rest = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(&trimmed);
    rest.split('/')
        .next()
        .filter(|host| !host.is_empty())
        .map(str::to_string)
}

struct Stored {
    manual: HashMap<String, bool>,
    cache: Map<String, Value>,
}

async fn stored(pool: &MySqlPool, ids: &[i32]) -> sqlx::Result<HashMap<i32, Stored>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut query = sqlx::QueryBuilder::new(
        "SELECT company_id, CAST(manual_overrides AS CHAR), CAST(auto_cache AS CHAR)
           FROM company_setup_checklist WHERE company_id IN (",
    );
    let mut list = query.separated(", ");
    for id in ids {
        list.push_bind(id);
    }
    query.push(")");
    let rows: Vec<(i32, Option<String>, Option<String>)> =
        query.build_query_as().fetch_all(pool).await?;
    let object = |json: Option<String>| -> Map<String, Value> {
        json.and_then(|json| serde_json::from_str::<Value>(&json).ok())
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default()
    };
    Ok(rows
        .into_iter()
        .map(|(id, manual, cache)| {
            let manual = object(manual)
                .into_iter()
                .filter(|(key, _)| ITEM_KEYS.contains(&key.as_str()))
                .filter_map(|(key, value)| value.as_bool().map(|on| (key, on)))
                .collect();
            (
                id,
                Stored {
                    manual,
                    cache: object(cache),
                },
            )
        })
        .collect())
}

async fn positions(pool: &MySqlPool, ids: &[i32]) -> sqlx::Result<HashMap<i32, HashSet<i32>>> {
    let mut map: HashMap<i32, HashSet<i32>> = HashMap::new();
    if ids.is_empty() {
        return Ok(map);
    }
    let mut query = sqlx::QueryBuilder::new(
        "SELECT u.company_id, up.company_id, up.position_id
           FROM users u JOIN users_positions up ON up.user_id = u.id
          WHERE u.is_deleted = 0 AND (u.company_id IN (",
    );
    let mut list = query.separated(", ");
    for id in ids {
        list.push_bind(id);
    }
    query.push(") OR up.company_id IN (");
    let mut list = query.separated(", ");
    for id in ids {
        list.push_bind(id);
    }
    query.push("))");
    let rows: Vec<(Option<i32>, Option<i32>, i32)> = query.build_query_as().fetch_all(pool).await?;
    for (home, access, position) in rows {
        for company in [home, access].into_iter().flatten() {
            map.entry(company).or_default().insert(position);
        }
    }
    Ok(map)
}

/// The app's base domain (`granite-manager.com`), from `APP_BASE_DOMAIN`
/// or `APP_URL`.
pub fn base_domain(state: &AppState) -> String {
    if let Ok(base) = std::env::var("APP_BASE_DOMAIN") {
        let base = base.trim().trim_start_matches('.').to_lowercase();
        if !base.is_empty() && base != "localhost" && !base.ends_with(".localhost") {
            return base;
        }
    }
    url::Url::parse(&state.app_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_lowercase))
        .map(|host| host.trim_start_matches("www.").to_string())
        .filter(|host| host != "localhost" && !host.ends_with(".localhost") && host.contains('.'))
        .unwrap_or_else(|| "granite-manager.com".into())
}

async fn resolves(dns: &impl Dns, host: &str) -> bool {
    let lookup = async {
        dns.a(host).await.is_ok_and(|ips| !ips.is_empty()) || !dns.cname(host).await.is_empty()
    };
    tokio::time::timeout(std::time::Duration::from_secs(3), lookup)
        .await
        .unwrap_or(false)
}

fn item(key: &str, auto: Option<bool>, manual: Option<bool>, detail: Option<String>) -> Item {
    let auto = auto == Some(true);
    let manual = manual == Some(true);
    Item {
        key: key.to_string(),
        label: label(key).to_string(),
        done: auto || manual,
        auto,
        manual,
        source: if auto {
            "auto"
        } else if manual {
            "manual"
        } else {
            "none"
        }
        .into(),
        detail,
    }
}

/// The checklists of `companies`, checking subdomain DNS and SES now.
#[allow(clippy::too_many_lines)]
pub async fn checklists(
    state: &AppState,
    companies: &[CompanyFacts],
) -> sqlx::Result<HashMap<i32, Checklist>> {
    let ids: Vec<i32> = companies.iter().map(|c| c.id).collect();
    let (stored, positions) =
        tokio::try_join!(stored(&state.pool, &ids), positions(&state.pool, &ids))?;
    let any_domain = companies
        .iter()
        .any(|c| domain_host(c.domain.as_deref()).is_some());
    let verified = if any_domain {
        state
            .ses
            .sets()
            .await
            .map(|(verified, _)| verified)
            .unwrap_or_default()
    } else {
        HashSet::new()
    };
    let base = base_domain(state);
    let infra = futures_util::future::join_all(companies.iter().map(|company| {
        let subdomain = company
            .subdomain
            .as_deref()
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty());
        let base = base.clone();
        let self_serve = company.self_serve;
        async move {
            match subdomain {
                Some(subdomain) => resolves(&state.dns, &format!("{subdomain}.{base}")).await,
                None => self_serve,
            }
        }
    }))
    .await;
    let empty = Stored {
        manual: HashMap::new(),
        cache: Map::new(),
    };
    Ok(companies
        .iter()
        .zip(infra)
        .map(|(company, subdomain_infra)| {
            let stored = stored.get(&company.id).unwrap_or(&empty);
            let manual = |key: &str| stored.manual.get(key).copied();
            let held = positions.get(&company.id);
            let domain = domain_host(company.domain.as_deref());
            let subdomain = company
                .subdomain
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let mut items = vec![
                item(
                    "subdomain_infra",
                    Some(subdomain_infra),
                    manual("subdomain_infra"),
                    Some(subdomain.map_or_else(
                        || {
                            if company.self_serve {
                                "Not needed: this company has no subdomain".to_string()
                            } else {
                                "Set subdomain first".to_string()
                            }
                        },
                        |s| format!("Expect DNS for {s}.granite-manager.com"),
                    )),
                ),
                item(
                    "domain",
                    Some(domain.is_some()),
                    manual("domain"),
                    domain.clone(),
                ),
            ];
            for (key, position) in USER_POSITIONS {
                items.push(item(
                    key,
                    Some(held.is_some_and(|held| held.contains(&position))),
                    manual(key),
                    None,
                ));
            }
            items.push(item(
                "inbound_email",
                Some(domain.as_ref().is_some_and(|d| verified.contains(d))),
                manual("inbound_email"),
                Some(domain.as_ref().map_or_else(
                    || "Set domain first".to_string(),
                    |d| format!("SES identity: {d}"),
                )),
            ));
            items.push(item(
                "stones_website",
                stored.cache.get("stones_website").and_then(Value::as_bool),
                manual("stones_website"),
                Some("Look for granite-manager customer stones embed on /stones or /stone".into()),
            ));
            for key in [
                "onboarding.manager",
                "onboarding.inventory_manager",
                "onboarding.sales_reps",
            ] {
                items.push(item(key, None, manual(key), None));
            }
            if company.quickbooks_enabled {
                items.push(item(
                    "quickbooks",
                    Some(
                        company
                            .qbo_realm_id
                            .as_deref()
                            .is_some_and(|r| !r.trim().is_empty()),
                    ),
                    manual("quickbooks"),
                    None,
                ));
            }
            items.push(item(
                "cloudtalk",
                Some(company.cloudtalk),
                manual("cloudtalk"),
                None,
            ));
            items.push(item(
                "ringcentral",
                Some(company.ringcentral_jwt_set),
                manual("ringcentral"),
                None,
            ));
            let done_count = items.iter().filter(|i| i.done).count();
            (
                company.id,
                Checklist {
                    company_id: company.id,
                    total_count: items.len(),
                    done_count,
                    items,
                },
            )
        })
        .collect())
}

/// Ticks or unticks an item by hand.
pub async fn set_manual(
    pool: &MySqlPool,
    company_id: i32,
    key: &str,
    checked: bool,
) -> sqlx::Result<()> {
    let current: Option<Option<String>> = sqlx::query_scalar(
        "SELECT CAST(manual_overrides AS CHAR) FROM company_setup_checklist WHERE company_id = ?",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    let mut manual: Map<String, Value> = current
        .clone()
        .flatten()
        .and_then(|json| serde_json::from_str::<Value>(&json).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter(|(k, v)| ITEM_KEYS.contains(&k.as_str()) && v.is_boolean())
        .collect();
    if checked {
        manual.insert(key.to_string(), Value::Bool(true));
    } else {
        manual.remove(key);
    }
    let json = Value::Object(manual).to_string();
    if current.is_some() {
        sqlx::query("UPDATE company_setup_checklist SET manual_overrides = ? WHERE company_id = ?")
            .bind(json)
            .bind(company_id)
            .execute(pool)
            .await?;
    } else {
        sqlx::query(
            "INSERT INTO company_setup_checklist (company_id, manual_overrides, auto_cache)
             VALUES (?, ?, CAST('{}' AS JSON))",
        )
        .bind(company_id)
        .bind(json)
        .execute(pool)
        .await?;
    }
    Ok(())
}
