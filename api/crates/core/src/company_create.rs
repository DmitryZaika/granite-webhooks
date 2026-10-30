//! A new company with its seeds, and its first owner: shared by
//! `POST /v1/superuser/companies` and self-serve signup provisioning.

use crate::error::{ApiError, ApiResult};
use serde::Deserialize;
use sqlx::MySqlConnection;
use std::fmt::Write as _;
use std::sync::LazyLock;

/// Subdomains that are Granite Manager's own hosts, never a company's.
pub const RESERVED_SUBDOMAINS: &[&str] = &[
    "payments", "api", "api-dev", "stream", "www", "docs", "dev", "test", "new", "app",
];

/// Built-in calendars every company has: slug, name, color.
pub const SYSTEM_CALENDARS: [(&str, &str, &str); 3] = [
    ("templates", "Templates", "green"),
    ("installations", "Installations", "red"),
    ("estimates", "Estimates", "blue"),
];

/// `company.plan_source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanSource {
    /// Everything free as today (superuser-created companies).
    Legacy,
    /// Self-serve: add-ons by subscription; Stripe payment required.
    Subscription,
}

/// A company to create. Text fields are already trimmed (empty = `None`).
#[derive(Debug, Clone)]
pub struct CompanyInit {
    pub name: String,
    /// `None` for self-serve companies (they live on the apex).
    pub subdomain: Option<String>,
    pub address: Option<String>,
    /// `company.state_taxes` as stored (`"0.0700"`).
    pub state_taxes: Option<String>,
    pub domain: Option<String>,
    pub cloudtalk_access_key: Option<String>,
    pub cloudtalk_access_secret: Option<String>,
    pub ringcentral_client_id: Option<String>,
    pub ringcentral_client_secret: Option<String>,
    pub ringcentral_jwt: Option<String>,
    pub ringcentral_server_url: Option<String>,
    pub plan_source: PlanSource,
}

/// Inserts the company and its seeds on `conn`, inside the caller's transaction.
///
/// Seeds: system calendars, default email templates with the apex inventory
/// link, customer sources, and the Walk-in system user with an unusable
/// password. A subscription company also gets `plan_source =
/// 'subscription'`, `billing_required = 1`, `billing_required_at = now` and
/// no user limit (`max_users = NULL`: it pays per user).
pub async fn create_company_tx(conn: &mut MySqlConnection, init: &CompanyInit) -> ApiResult<i32> {
    let subscription = init.plan_source == PlanSource::Subscription;
    let sql = if subscription {
        "INSERT INTO company (name, subdomain, address, state_taxes, domain, cloudtalk_access_key,
                              cloudtalk_access_secret, ringcentral_client_id, ringcentral_client_secret,
                              ringcentral_jwt, ringcentral_server_url, statistics_widgets_hidden,
                              plan_source, billing_required, billing_required_at, max_users)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, 'subscription', 1, UTC_TIMESTAMP(), NULL)"
    } else {
        "INSERT INTO company (name, subdomain, address, state_taxes, domain, cloudtalk_access_key,
                              cloudtalk_access_secret, ringcentral_client_id, ringcentral_client_secret,
                              ringcentral_jwt, ringcentral_server_url, statistics_widgets_hidden)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1)"
    };
    let id = sqlx::query(sql)
        .bind(&init.name)
        .bind(&init.subdomain)
        .bind(&init.address)
        .bind(&init.state_taxes)
        .bind(&init.domain)
        .bind(&init.cloudtalk_access_key)
        .bind(&init.cloudtalk_access_secret)
        .bind(&init.ringcentral_client_id)
        .bind(&init.ringcentral_client_secret)
        .bind(&init.ringcentral_jwt)
        .bind(&init.ringcentral_server_url)
        .execute(&mut *conn)
        .await?
        .last_insert_id();
    let id = i32::try_from(id).map_err(|_| ApiError::Internal("company id out of range".into()))?;
    for (slug, name, color) in SYSTEM_CALENDARS {
        sqlx::query("INSERT INTO calendars (company_id, name, color, slug, created_by) VALUES (?, ?, ?, ?, NULL)")
            .bind(id)
            .bind(name)
            .bind(color)
            .bind(slug)
            .execute(&mut *conn)
            .await?;
    }
    for template in DEFAULT_TEMPLATES.iter() {
        sqlx::query(
            "INSERT INTO email_templates (template_name, template_subject, template_body, company_id, user_id)
             VALUES (?, ?, ?, ?, NULL)",
        )
        .bind(&template.name)
        .bind(&template.subject)
        .bind(&template.body)
        .bind(id)
        .execute(&mut *conn)
        .await?;
    }
    let walk_in_email = init.subdomain.as_ref().map_or_else(
        || format!("walk-in-{id}@granite-manager.invalid"),
        |subdomain| format!("walk-in@{subdomain}.com"),
    );
    sqlx::query(
        "INSERT INTO users (name, email, password, company_id, is_employee, is_admin, is_system)
         VALUES ('Walk-in', ?, ?, ?, 1, 0, 1)",
    )
    .bind(walk_in_email)
    .bind(unusable_password()?)
    .bind(id)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO customer_sources (company_id, source_key, label, is_system) VALUES
           (?, 'check-in', 'Walk-in', 1), (?, 'leads', 'Leads', 1),
           (?, 'call-in', 'Call-in', 1), (?, 'other', 'Other', 1)",
    )
    .bind(id)
    .bind(id)
    .bind(id)
    .bind(id)
    .execute(&mut *conn)
    .await?;
    Ok(id)
}

static DEFAULT_TEMPLATES: LazyLock<Vec<DefaultTemplate>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../../../data/default_email_templates.json"))
        .expect("valid default templates")
});

#[derive(Deserialize)]
struct DefaultTemplate {
    name: String,
    subject: String,
    body: String,
}

/// A stored password nobody can sign in with: not a bcrypt hash, so
/// `bcrypt::verify` fails.
fn unusable_password() -> ApiResult<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| ApiError::Internal("no randomness".into()))?;
    let mut password = String::from("!");
    for byte in bytes {
        let _ = write!(password, "{byte:02x}");
    }
    Ok(password)
}

/// The company's first user.
pub struct OwnerInit<'a> {
    pub name: &'a str,
    /// Lowercase, validated.
    pub email: &'a str,
    /// bcrypt hash.
    pub password_hash: &'a str,
    pub phone: Option<&'a str>,
}

/// Inserts the owner: Company Admin (`is_admin = 1`, `is_employee = 1`) and
/// Manager (position 5) of `company_id`. Answers the user id.
pub async fn create_owner_tx(
    conn: &mut MySqlConnection,
    company_id: i32,
    owner: &OwnerInit<'_>,
) -> sqlx::Result<i32> {
    let id = sqlx::query(
        "INSERT INTO users (name, phone_number, email, password, company_id, is_employee, is_admin, is_system)
         VALUES (?, ?, ?, ?, ?, 1, 1, 0)",
    )
    .bind(owner.name)
    .bind(owner.phone)
    .bind(owner.email)
    .bind(owner.password_hash)
    .bind(company_id)
    .execute(&mut *conn)
    .await?
    .last_insert_id();
    let id = i32::try_from(id).map_err(|_| sqlx::Error::Protocol("user id out of range".into()))?;
    sqlx::query("INSERT INTO users_positions (user_id, position_id, company_id) VALUES (?, 5, ?)")
        .bind(id)
        .bind(company_id)
        .execute(&mut *conn)
        .await?;
    Ok(id)
}

/// A valid company subdomain: lowercase letters and digits in hyphen-separated
/// parts, not one of [`RESERVED_SUBDOMAINS`].
pub fn valid_subdomain(value: &str) -> bool {
    !value.is_empty()
        && !RESERVED_SUBDOMAINS.contains(&value)
        && value.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}
