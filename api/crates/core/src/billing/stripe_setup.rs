//! The Stripe account setup behind `examples/stripe_setup.rs`: the five
//! self-serve prices, the customer portal configuration and the webhook
//! endpoint (03-stripe.md sections 4.1, 4.7, 4.9).
//!
//! [`run`] reads the account, prints what exists and what is wrong, and with
//! `apply` creates what is missing. It is idempotent: things are found by
//! lookup key, tag or URL, and every write carries a stable idempotency key.
//! A price cannot be edited in Stripe, so a wrong one is reported and never
//! touched.

use super::account::{PORTAL_CONFIGURATION_NAME, PORTAL_CONFIGURATION_TAG};
use super::catalog::{BASE_LOOKUP_KEY, EXTRA_USER_LOOKUP_KEY, INCLUDED_USERS};
use super::setup::{KeyMode, REQUIRED_EVENTS, WEBHOOK_PATH};
use super::stripe::{API_VERSION, Stripe, StripeError};
use crate::entitlements::AddonKey;
use crate::signup::consent::STATED_EXTRA_USER_CENTS;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};

/// The base price, in cents.
pub const BASE_CENTS: i64 = 30_000;

/// The monthly price of an add-on, in cents.
pub const fn addon_cents(key: AddonKey) -> i64 {
    match key {
        AddonKey::Signatures => 1_500,
        AddonKey::Quotes => 7_500,
        AddonKey::Quickbooks => 3_000,
    }
}

const ALLOWED_CUSTOMER_UPDATES: [&str; 5] = ["name", "email", "address", "phone", "tax_id"];

#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("Stripe: {0}")]
    Stripe(#[from] StripeError),
    #[error("{0}")]
    Refused(String),
    #[error("cannot write output: {0}")]
    Io(#[from] std::io::Error),
}

/// What the script was asked to do.
#[derive(Debug, Clone)]
pub struct Options {
    /// Write to Stripe; otherwise only read and report.
    pub apply: bool,
    /// The webhook endpoint's URL.
    pub webhook_url: String,
    /// Where a newly created endpoint's signing secret is written (mode 0600).
    pub secret_file: Option<PathBuf>,
    /// The payments app's origin (the portal returns to its account page).
    pub payments_app_url: String,
    /// The marketing site's origin (terms and privacy pages).
    pub app_url: String,
}

impl Options {
    /// The URL of the production api's webhook route.
    pub fn default_webhook_url() -> String {
        format!("https://api.granite-manager.com{WEBHOOK_PATH}")
    }

    /// Defaults from the environment, like the api reads them.
    pub fn from_env(
        apply: bool,
        webhook_url: Option<String>,
        secret_file: Option<PathBuf>,
    ) -> Self {
        let app_url = std::env::var("APP_URL")
            .ok()
            .map(|url| url.trim().trim_end_matches('/').to_string())
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| "https://granite-manager.com".into());
        Self {
            apply,
            webhook_url: webhook_url.unwrap_or_else(Self::default_webhook_url),
            secret_file,
            payments_app_url: super::payments_app_url(),
            app_url,
        }
    }
}

/// What a run found and did.
#[derive(Debug, Default)]
pub struct Outcome {
    /// Things created or updated (with `apply`) or that `apply` would create or update.
    pub changes: usize,
    /// Things wrong that the script will not change: a human decides.
    pub problems: usize,
    /// `(lookup_key, price id)` of the prices that exist after the run (or exist now).
    pub price_ids: Vec<(String, String)>,
    pub portal_configuration_id: Option<String>,
    pub webhook_endpoint_id: Option<String>,
    /// The file the new signing secret was written to.
    pub secret_file_written: Option<PathBuf>,
}

/// One self-serve price (03-stripe.md section 4.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriceSpec {
    pub lookup_key: &'static str,
    pub product_name: String,
    pub cents: i64,
    /// The price's `metadata` entry (`kind=base`, `addon_key=quotes`).
    pub tag: (&'static str, &'static str),
}

pub fn price_specs() -> Vec<PriceSpec> {
    let mut specs = vec![
        PriceSpec {
            lookup_key: BASE_LOOKUP_KEY,
            product_name: format!("Granite Manager Base (up to {INCLUDED_USERS} users)"),
            cents: BASE_CENTS,
            tag: ("kind", "base"),
        },
        PriceSpec {
            lookup_key: EXTRA_USER_LOOKUP_KEY,
            product_name: "Extra user".into(),
            cents: STATED_EXTRA_USER_CENTS,
            tag: ("kind", "seat"),
        },
    ];
    specs.extend(AddonKey::ALL.map(|key| PriceSpec {
        lookup_key: key.lookup_key(),
        product_name: key.label().into(),
        cents: addon_cents(key),
        tag: ("addon_key", key.as_str()),
    }));
    specs
}

fn money(cents: i64) -> String {
    format!("{}.{:02}", cents / 100, cents % 100)
}

/// `whsec_abcdefgh1234` as `whsec_…1234`.
pub fn mask_secret(secret: &str) -> String {
    let prefix = secret.split('_').next().unwrap_or_default();
    let tail: String = secret
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{prefix}_…{tail}")
}

/// What is wrong with an existing price: `(fields Stripe cannot edit, metadata)`.
pub fn price_faults(spec: &PriceSpec, price: &Value) -> (Vec<String>, Vec<String>) {
    let mut fixed = Vec::new();
    if price["unit_amount"].as_i64() != Some(spec.cents) {
        fixed.push(format!(
            "amount is {} cents, expected {}",
            price["unit_amount"]
                .as_i64()
                .map_or_else(|| "unset".into(), |cents| cents.to_string()),
            spec.cents
        ));
    }
    let tax = price["tax_behavior"].as_str().unwrap_or("unset");
    if tax != "exclusive" {
        fixed.push(format!("tax_behavior is {tax}, expected exclusive"));
    }
    let currency = price["currency"].as_str().unwrap_or("unset");
    if currency != "usd" {
        fixed.push(format!("currency is {currency}, expected usd"));
    }
    let recurring = &price["recurring"];
    if recurring["interval"] != "month" || recurring["interval_count"].as_i64().unwrap_or(1) != 1 {
        fixed.push("not a monthly price".into());
    }
    if price["billing_scheme"] != "per_unit" {
        fixed.push("billing_scheme is not per_unit".into());
    }
    let (name, value) = spec.tag;
    let mut metadata = Vec::new();
    if price["metadata"][name] != value {
        metadata.push(format!("metadata {name} should be {value}"));
    }
    (fixed, metadata)
}

fn price_form(spec: &PriceSpec) -> Vec<(&'static str, String)> {
    let tag = if spec.tag.0 == "kind" {
        "metadata[kind]"
    } else {
        "metadata[addon_key]"
    };
    vec![
        ("currency", "usd".into()),
        ("unit_amount", spec.cents.to_string()),
        ("recurring[interval]", "month".into()),
        ("billing_scheme", "per_unit".into()),
        ("tax_behavior", "exclusive".into()),
        ("lookup_key", spec.lookup_key.into()),
        ("product_data[name]", spec.product_name.clone()),
        (tag, spec.tag.1.to_string()),
    ]
}

/// The portal configuration of 03-stripe.md section 4.7.
pub fn portal_form(options: &Options) -> Vec<(&'static str, String)> {
    let mut form = vec![
        ("name", PORTAL_CONFIGURATION_NAME.to_string()),
        ("metadata[app]", PORTAL_CONFIGURATION_TAG.to_string()),
        (
            "default_return_url",
            format!("{}/account", options.payments_app_url),
        ),
        (
            "business_profile[terms_of_service_url]",
            format!("{}/terms", options.app_url),
        ),
        (
            "business_profile[privacy_policy_url]",
            format!("{}/privacy-policy", options.app_url),
        ),
        ("features[payment_method_update][enabled]", "true".into()),
        ("features[invoice_history][enabled]", "true".into()),
        ("features[customer_update][enabled]", "true".into()),
    ];
    form.extend(ALLOWED_CUSTOMER_UPDATES.map(|field| {
        (
            "features[customer_update][allowed_updates][]",
            field.to_string(),
        )
    }));
    form.push(("features[subscription_cancel][enabled]", "false".into()));
    form.push(("features[subscription_update][enabled]", "false".into()));
    form
}

/// How an existing portal configuration differs from [`portal_form`].
pub fn portal_differences(config: &Value, options: &Options) -> Vec<String> {
    let mut found = Vec::new();
    let mut text = |label: &str, actual: &Value, expected: String| {
        if actual.as_str() != Some(expected.as_str()) {
            found.push(format!(
                "{label} is {}, expected {expected}",
                actual.as_str().unwrap_or("unset")
            ));
        }
    };
    text("name", &config["name"], PORTAL_CONFIGURATION_NAME.into());
    text(
        "default_return_url",
        &config["default_return_url"],
        format!("{}/account", options.payments_app_url),
    );
    text(
        "terms_of_service_url",
        &config["business_profile"]["terms_of_service_url"],
        format!("{}/terms", options.app_url),
    );
    text(
        "privacy_policy_url",
        &config["business_profile"]["privacy_policy_url"],
        format!("{}/privacy-policy", options.app_url),
    );
    for (feature, wanted) in [
        ("payment_method_update", true),
        ("invoice_history", true),
        ("customer_update", true),
        ("subscription_cancel", false),
        ("subscription_update", false),
    ] {
        let enabled = config["features"][feature]["enabled"].as_bool();
        if enabled != Some(wanted) {
            found.push(format!(
                "{feature} is {}, expected {}",
                enabled.map_or("unset", |on| if on { "on" } else { "off" }),
                if wanted { "on" } else { "off" }
            ));
        }
    }
    let mut allowed: Vec<&str> = config["features"]["customer_update"]["allowed_updates"]
        .as_array()
        .map(|list| list.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    allowed.sort_unstable();
    let mut wanted = ALLOWED_CUSTOMER_UPDATES.to_vec();
    wanted.sort_unstable();
    if allowed != wanted {
        found.push(format!(
            "customer_update allows [{}], expected [{}]",
            allowed.join(", "),
            wanted.join(", ")
        ));
    }
    found
}

/// The events an endpoint lacks and the ones it has beyond [`REQUIRED_EVENTS`]
/// (`*` counts as extra).
pub fn event_difference(endpoint: &Value) -> (Vec<String>, Vec<String>) {
    let have: Vec<&str> = endpoint["enabled_events"]
        .as_array()
        .map(|list| list.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let missing = REQUIRED_EVENTS
        .into_iter()
        .filter(|event| !have.contains(event))
        .map(str::to_string)
        .collect();
    let extra = have
        .into_iter()
        .filter(|event| !REQUIRED_EVENTS.contains(event))
        .map(str::to_string)
        .collect();
    (missing, extra)
}

fn webhook_form(url: Option<&str>) -> Vec<(&'static str, String)> {
    let mut form = Vec::new();
    if let Some(url) = url {
        form.push(("url", url.to_string()));
        form.push(("api_version", API_VERSION.to_string()));
        form.push(("description", "Granite Manager billing (api)".to_string()));
    }
    form.extend(REQUIRED_EVENTS.map(|event| ("enabled_events[]", event.to_string())));
    form
}

/// `setup:<label>:<hash of the request>:v1`: the same request always gets the
/// same key, a changed one a new key (Stripe refuses a key reused with other
/// parameters).
fn stable_key(label: &str, form: &[(&str, String)]) -> String {
    let body = serde_html_form::to_string(form).unwrap_or_default();
    let digest = Sha256::digest(body.as_bytes());
    let short = digest
        .iter()
        .take(6)
        .fold(0_u64, |acc, byte| (acc << 8) | u64::from(*byte));
    format!("setup:{label}:{short:012x}:v1")
}

fn same_url(a: &str, b: &str) -> bool {
    a.trim().trim_end_matches('/') == b.trim().trim_end_matches('/')
}

/// Creates the (empty) secret file with mode 0600, failing when it exists, so
/// a signing secret never lands in a file that cannot take it.
fn reserve_secret_file(path: &Path) -> std::io::Result<()> {
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.mode(0o600);
    }
    open.open(path).map(drop)
}

enum WebhookPlan {
    Create,
    Existing(Value),
}

struct Run<'a> {
    stripe: &'a Stripe,
    options: &'a Options,
    out: &'a mut dyn Write,
    outcome: Outcome,
}

impl Run<'_> {
    fn say(&mut self, tag: &str, text: &str) -> Result<(), SetupError> {
        writeln!(self.out, "  {tag:<13} {}", text.trim_end())?;
        Ok(())
    }

    fn heading(&mut self, text: &str) -> Result<(), SetupError> {
        writeln!(self.out, "\n{text}")?;
        Ok(())
    }

    /// Records a change and says it as done or as what `--apply` would do.
    fn change(&mut self, done: &str, would: &str, text: &str) -> Result<(), SetupError> {
        self.outcome.changes += 1;
        let tag = if self.options.apply { done } else { would };
        self.say(tag, text)
    }

    fn problem(&mut self, text: &str) -> Result<(), SetupError> {
        self.outcome.problems += 1;
        self.say("PROBLEM", text)
    }

    async fn prices(&mut self) -> Result<(), SetupError> {
        self.heading("Prices (monthly, USD, per unit, tax exclusive)")?;
        let specs = price_specs();
        let keys: Vec<&str> = specs.iter().map(|spec| spec.lookup_key).collect();
        let existing = self.stripe.prices_by_lookup_keys(&keys).await?;
        for spec in &specs {
            let label = format!("{} {} USD", spec.lookup_key, money(spec.cents));
            let Some(price) = existing
                .iter()
                .find(|price| price["lookup_key"] == spec.lookup_key)
            else {
                let created = if self.options.apply {
                    let form = price_form(spec);
                    let key = format!("setup:{}:v1", spec.lookup_key);
                    Some(self.stripe.create_price_keyed(&form, &key).await?)
                } else {
                    None
                };
                let id = created
                    .as_ref()
                    .and_then(|price| price["id"].as_str())
                    .unwrap_or_default()
                    .to_string();
                self.change("created", "would create", &format!("{label} {id}"))?;
                if !id.is_empty() {
                    self.outcome.price_ids.push((spec.lookup_key.into(), id));
                }
                continue;
            };
            let id = price["id"].as_str().unwrap_or_default().to_string();
            let (fixed, metadata) = price_faults(spec, price);
            if fixed.is_empty() && metadata.is_empty() {
                self.say("ok", &format!("{label} {id}"))?;
            }
            if !fixed.is_empty() {
                self.problem(&format!(
                    "{label} {id}: {}. A price cannot be edited and this script will not change it. \
                     Creating a replacement (lookup key {}_v2, then update the lookup key constant in the api) \
                     is a human decision; subscriptions on {id} keep it until migrated.",
                    fixed.join("; "),
                    spec.lookup_key
                ))?;
            }
            if !metadata.is_empty() {
                self.problem(&format!(
                    "{label} {id}: {}. Fix it in the Dashboard (metadata can be edited).",
                    metadata.join("; ")
                ))?;
            }
            self.outcome.price_ids.push((spec.lookup_key.into(), id));
        }
        Ok(())
    }

    async fn portal(&mut self) -> Result<(), SetupError> {
        self.heading("Customer portal configuration (no cancel, no plan change)")?;
        let form = portal_form(self.options);
        let configurations = self.stripe.list_portal_configurations().await?;
        let tagged = configurations
            .iter()
            .find(|config| config["metadata"]["app"] == PORTAL_CONFIGURATION_TAG);
        let Some(config) = tagged else {
            let created = if self.options.apply {
                let key = stable_key("portal", &form);
                Some(
                    self.stripe
                        .create_portal_configuration_keyed(&form, &key)
                        .await?,
                )
            } else {
                None
            };
            let id = created
                .as_ref()
                .and_then(|config| config["id"].as_str())
                .unwrap_or_default()
                .to_string();
            self.change(
                "created",
                "would create",
                &format!("{PORTAL_CONFIGURATION_TAG} {id}"),
            )?;
            if !id.is_empty() {
                self.outcome.portal_configuration_id = Some(id);
            }
            return Ok(());
        };
        let id = config["id"].as_str().unwrap_or_default().to_string();
        let differences = portal_differences(config, self.options);
        if differences.is_empty() {
            self.say("ok", &format!("{PORTAL_CONFIGURATION_TAG} {id}"))?;
        } else {
            for difference in &differences {
                self.say("differs", difference)?;
            }
            if self.options.apply {
                let key = stable_key(&format!("portal-update-{id}"), &form);
                self.stripe
                    .update_portal_configuration(&id, &form, &key)
                    .await?;
            }
            self.change(
                "updated",
                "would update",
                &format!("{PORTAL_CONFIGURATION_TAG} {id}"),
            )?;
        }
        self.outcome.portal_configuration_id = Some(id);
        Ok(())
    }

    async fn webhook(&mut self, plan: WebhookPlan) -> Result<(), SetupError> {
        self.heading(&format!(
            "Webhook endpoint ({}, api version {API_VERSION})",
            self.options.webhook_url
        ))?;
        let url = self.options.webhook_url.clone();
        let endpoint = match plan {
            WebhookPlan::Create => {
                let form = webhook_form(Some(&url));
                let created = if self.options.apply {
                    let key = stable_key("webhook", &form);
                    Some(self.stripe.create_webhook_endpoint(&form, &key).await?)
                } else {
                    None
                };
                let id = created
                    .as_ref()
                    .and_then(|endpoint| endpoint["id"].as_str())
                    .unwrap_or_default()
                    .to_string();
                self.change(
                    "created",
                    "would create",
                    &format!("{id} with {} events", REQUIRED_EVENTS.len()),
                )?;
                if let Some(created) = &created {
                    self.keep_secret(created)?;
                    self.outcome.webhook_endpoint_id = Some(id);
                }
                return Ok(());
            }
            WebhookPlan::Existing(endpoint) => endpoint,
        };
        let id = endpoint["id"].as_str().unwrap_or_default().to_string();
        self.outcome.webhook_endpoint_id = Some(id.clone());
        let mut blocked = false;
        if endpoint["status"] != "enabled" {
            self.problem(&format!(
                "{id} is {}: enable it in the Dashboard (Developers, Webhooks).",
                endpoint["status"].as_str().unwrap_or("not enabled")
            ))?;
        }
        let version = endpoint["api_version"].as_str();
        if version != Some(API_VERSION) {
            blocked = true;
            self.problem(&format!(
                "{id} has api_version {}, the api needs {API_VERSION}. Stripe cannot change it on an \
                 existing endpoint: delete {id} in the Dashboard and run this script again to create a new one.",
                version.unwrap_or("account default")
            ))?;
        }
        let (missing, extra) = event_difference(&endpoint);
        if missing.is_empty() && extra.is_empty() {
            self.say(
                "ok",
                &format!("{id} sends exactly the {} events", REQUIRED_EVENTS.len()),
            )?;
        } else {
            if !missing.is_empty() {
                self.say(
                    "differs",
                    &format!("missing events: {}", missing.join(", ")),
                )?;
            }
            if !extra.is_empty() {
                self.say("differs", &format!("extra events: {}", extra.join(", ")))?;
            }
            if !blocked {
                if self.options.apply {
                    let form = webhook_form(None);
                    let key = stable_key(&format!("webhook-update-{id}"), &form);
                    self.stripe
                        .update_webhook_endpoint(&id, &form, &key)
                        .await?;
                }
                self.change("updated", "would update", &format!("{id} events"))?;
            }
        }
        Ok(())
    }

    /// Writes the signing secret of a new endpoint to the secret file and
    /// says it masked.
    fn keep_secret(&mut self, endpoint: &Value) -> Result<(), SetupError> {
        let Some(secret) = endpoint["secret"].as_str() else {
            self.problem("Stripe did not return a signing secret: reveal it in the Dashboard.")?;
            return Ok(());
        };
        let Some(path) = self.options.secret_file.clone() else {
            return Err(SetupError::Refused(
                "the endpoint was created but no --secret-file was given".into(),
            ));
        };
        std::fs::write(&path, format!("{secret}\n"))?;
        self.say(
            "secret",
            &format!("{} written to {}", mask_secret(secret), path.display()),
        )?;
        self.outcome.secret_file_written = Some(path);
        Ok(())
    }

    fn environment(&mut self, mode: KeyMode) -> Result<(), SetupError> {
        let public = match mode {
            KeyMode::Live => "pk_live_",
            KeyMode::Test => "pk_test_",
        };
        self.heading(&format!(
            "Environment of the granite-api Lambda ({} mode)",
            mode.name()
        ))?;
        let secret = match (&self.outcome.secret_file_written, self.options.apply) {
            (Some(path), _) => format!(
                "the contents of {} (delete the file after setting it)",
                path.display()
            ),
            (None, true) => {
                "unchanged: Stripe shows a signing secret only when the endpoint is created. Reveal \
                 or roll it in the Dashboard (Developers, Webhooks, the endpoint)."
                    .to_string()
            }
            (None, false) => "written to --secret-file when the endpoint is created".into(),
        };
        let lines = [
            (
                "STRIPE_SECRET_KEY",
                format!(
                    "a restricted key (rk_{}_...) is recommended (Customers, SetupIntents, Subscriptions, \
                     Subscription Items, Invoices, Invoice Items, Billing Portal, PaymentMethods, Prices, \
                     Webhook Endpoints read, Events read)",
                    mode.name()
                ),
            ),
            (
                "STRIPE_PUBLIC_KEY",
                format!(
                    "{public}... from Dashboard, Developers, API keys (not readable through the API)"
                ),
            ),
            ("STRIPE_WEBHOOK_SECRET", secret),
            (
                "PAYMENTS_APP_URL",
                format!(
                    "{} (the default; set only when different)",
                    self.options.payments_app_url
                ),
            ),
            (
                "APP_URL",
                format!(
                    "{} (the default; set only when different)",
                    self.options.app_url
                ),
            ),
            (
                "BILLING_ALERT_EMAIL",
                "where internal payment notices go".into(),
            ),
            (
                "SIGNUP_NOTIFY_USER_ID / SIGNUP_NOTIFY_EMAIL",
                "who is told about each new company".into(),
            ),
            (
                "CALENDLY_API_TOKEN",
                "Dima's Calendly, for the onboarding call".into(),
            ),
            (
                "ONBOARDING_ADVANCE_CHARGING",
                "1 only after the sandbox fee run".into(),
            ),
            (
                "SELF_SERVE_SIGNUP_ENABLED",
                "1 last, once GET /v1/superuser/billing/setup shows every check ok".into(),
            ),
        ];
        for (name, value) in lines {
            writeln!(self.out, "  {name}: {value}")?;
        }
        Ok(())
    }
}

/// Prints Stripe's `livemode` and refuses when it disagrees with the key.
async fn check_livemode(
    stripe: &Stripe,
    mode: KeyMode,
    out: &mut dyn Write,
) -> Result<(), SetupError> {
    match stripe.balance().await {
        Ok(balance) => {
            let live = balance["livemode"].as_bool();
            writeln!(
                out,
                "Stripe livemode: {}",
                live.map_or_else(|| "unknown".into(), |on| on.to_string())
            )?;
            if live.is_some_and(|live| live != (mode == KeyMode::Live)) {
                return Err(SetupError::Refused(
                    "the key's mode and Stripe's livemode disagree; nothing was changed".into(),
                ));
            }
        }
        Err(error) => writeln!(
            out,
            "Stripe livemode: could not read ({}), trusting the key prefix",
            error.code()
        )?,
    }
    Ok(())
}

/// Reads the account and, with `options.apply`, creates what is missing.
///
/// `mode` is the key's mode; Stripe's `livemode` must agree with it. Output
/// goes to `out`; a signing secret is only ever printed masked.
pub async fn run(
    stripe: &Stripe,
    mode: KeyMode,
    options: &Options,
    out: &mut dyn Write,
) -> Result<Outcome, SetupError> {
    writeln!(out, "Stripe key mode: {}", mode.name().to_uppercase())?;
    check_livemode(stripe, mode, out).await?;
    writeln!(
        out,
        "{}",
        if options.apply {
            "Mode: APPLY (writes to Stripe)"
        } else {
            "Mode: DRY RUN (reads only; add --apply to write)"
        }
    )?;

    let endpoints = stripe.webhook_endpoints().await?;
    let matching: Vec<&Value> = endpoints
        .iter()
        .filter(|endpoint| {
            same_url(
                endpoint["url"].as_str().unwrap_or_default(),
                &options.webhook_url,
            )
        })
        .collect();
    if matching.len() > 1 {
        return Err(SetupError::Refused(format!(
            "{} webhook endpoints have the URL {}: delete the extra ones in the Dashboard first",
            matching.len(),
            options.webhook_url
        )));
    }
    let plan = matching.first().map_or(WebhookPlan::Create, |endpoint| {
        WebhookPlan::Existing((*endpoint).clone())
    });
    if options.apply && matches!(plan, WebhookPlan::Create) {
        let Some(path) = &options.secret_file else {
            return Err(SetupError::Refused(
                "the webhook endpoint must be created and Stripe shows its signing secret once: \
                 pass --secret-file <path>; nothing was changed"
                    .into(),
            ));
        };
        reserve_secret_file(path).map_err(|error| {
            SetupError::Refused(format!(
                "cannot create the secret file {} ({error}); nothing was changed",
                path.display()
            ))
        })?;
    }

    let mut run = Run {
        stripe,
        options,
        out,
        outcome: Outcome::default(),
    };
    run.prices().await?;
    run.portal().await?;
    run.webhook(plan).await?;
    let others: Vec<String> = endpoints
        .iter()
        .filter(|endpoint| {
            !same_url(
                endpoint["url"].as_str().unwrap_or_default(),
                &options.webhook_url,
            )
        })
        .filter_map(|endpoint| endpoint["url"].as_str().map(str::to_string))
        .collect();
    if !others.is_empty() {
        run.say(
            "note",
            &format!(
                "{} other webhook endpoint(s) exist and were left alone: {}",
                others.len(),
                others.join(", ")
            ),
        )?;
    }
    run.environment(mode)?;

    let outcome = std::mem::take(&mut run.outcome);
    let summary = match (options.apply, outcome.changes, outcome.problems) {
        (false, 0, 0) => "Nothing to do.".to_string(),
        (false, changes, problems) => format!(
            "Dry run: {changes} change(s) would be made, {problems} problem(s) need a human. Re-run with --apply."
        ),
        (true, changes, problems) => {
            format!("Applied {changes} change(s); {problems} problem(s) need a human.")
        }
    };
    writeln!(out, "\n{summary}")?;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec() -> PriceSpec {
        price_specs().remove(0)
    }

    fn good_price() -> Value {
        json!({ "unit_amount": 30_000, "tax_behavior": "exclusive", "currency": "usd",
            "billing_scheme": "per_unit", "recurring": { "interval": "month", "interval_count": 1 },
            "metadata": { "kind": "base" } })
    }

    #[test]
    fn the_specs_are_the_five_catalog_prices() {
        let specs = price_specs();
        let rows: Vec<(&str, i64)> = specs.iter().map(|s| (s.lookup_key, s.cents)).collect();
        assert_eq!(
            rows,
            [
                ("granite_base_monthly_v1", 30_000),
                ("granite_extra_user_monthly_v1", 3_000),
                ("addon_signatures_monthly_v1", 1_500),
                ("addon_quotes_monthly_v1", 7_500),
                ("addon_quickbooks_monthly_v1", 3_000),
            ]
        );
        let form = price_form(&specs[4]);
        assert!(form.contains(&("metadata[addon_key]", "quickbooks".into())));
        assert!(form.contains(&("tax_behavior", "exclusive".into())));
    }

    #[test]
    fn immutable_price_faults_are_separate_from_metadata() {
        assert_eq!(price_faults(&spec(), &good_price()), (vec![], vec![]));
        let mut bad = good_price();
        bad["tax_behavior"] = json!("unspecified");
        bad["metadata"] = json!({});
        let (fixed, metadata) = price_faults(&spec(), &bad);
        assert_eq!(fixed, ["tax_behavior is unspecified, expected exclusive"]);
        assert_eq!(metadata, ["metadata kind should be base"]);
    }

    #[test]
    fn secrets_are_masked_to_their_last_four() {
        assert_eq!(mask_secret("whsec_abcdefgh1234"), "whsec_…1234");
    }

    #[test]
    fn the_same_request_gets_the_same_key() {
        let a = stable_key("portal", &[("a", "1".into())]);
        assert_eq!(a, stable_key("portal", &[("a", "1".into())]));
        assert_ne!(a, stable_key("portal", &[("a", "2".into())]));
    }
}
