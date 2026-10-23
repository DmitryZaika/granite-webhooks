use crate::ai::Ai;
use crate::billing::BillingPlan;
use crate::billing::stripe::Stripe;
use crate::maps::Maps;
use crate::qbo::Qbo;
use crate::storage::Storage;
use crate::telegram::TelegramBots;
use crate::transcribe::Transcriber;
use crate::webhooks_client::WebhooksClient;
use axum::http::{Method, header};
use sqlx::MySqlPool;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tower_http::cors::{AllowOrigin, CorsLayer};

/// Origins allowed when `API_ALLOWED_ORIGINS` is unset: the production tenant
/// subdomains. Local dev adds `http://localhost:5173` through the env file.
const DEFAULT_ALLOWED_ORIGINS: &str = "https://granite-manager.com,https://*.granite-manager.com";

#[derive(Clone)]
pub struct AppState {
    pub pool: MySqlPool,
    /// Same value as the Remix app's `SESSION_SECRET`; signs the `__session` cookie.
    pub session_secret: Arc<str>,
    pub allowed_origins: Arc<[String]>,
    /// Bucket for uploaded images.
    pub storage: Storage,
    /// The two Telegram bots (lead alerts, notifications), to check a user's
    /// link still works.
    pub telegram: TelegramBots,
    /// The `webhooks` Lambda, for work it owns (CloudTalk contact sync).
    pub webhooks: WebhooksClient,
    /// When each SMS provider and company last had its upkeep run.
    pub sms_upkeep: Arc<Mutex<HashMap<(&'static str, i32), Instant>>>,
    /// Recent texts sent per user, for the per-minute limit.
    pub sms_sends: Arc<Mutex<HashMap<i32, VecDeque<Instant>>>>,
    /// Recent SMS flow starts per user, for the per-minute limit.
    pub flow_starts: Arc<Mutex<HashMap<i32, VecDeque<Instant>>>>,
    /// Threads the composer's autocomplete gathered in the last minute.
    pub compose_contexts: Arc<crate::routes::ai::compose::ComposeContexts>,
    /// Google Maps, for driving distances.
    pub maps: Maps,
    /// Recent calls started per user, for the per-minute limit.
    pub call_starts: Arc<Mutex<HashMap<i32, VecDeque<Instant>>>>,
    /// HTTP client for the telephony providers' APIs.
    pub telephony_http: reqwest::Client,
    /// The `CloudTalk` API (a stub in tests).
    pub cloudtalk_base: Arc<str>,
    /// `OpenAI`, for the AI helpers.
    pub ai: Ai,
    /// The `SignWell` API (a stub in tests).
    pub signwell_base: Arc<str>,
    /// `AssemblyAI`, for call transcripts.
    pub transcriber: Transcriber,
    /// `QuickBooks` Online.
    pub qbo: Qbo,
    /// `LAMBDA_KEY`: the `Authorization` the scheduled jobs (`/v1/jobs/*`)
    /// must send. Jobs are refused when unset.
    pub job_key: Option<Arc<str>>,
    /// Where the contract templates are read from (a stub in tests).
    pub contract_templates_base: Arc<str>,
    /// Domain and `Secure` flag of the session cookie the auth routes set.
    pub cookie: crate::auth::cookie::CookieConfig,
    /// The app's public URL (`APP_URL`), the base of links sent by email.
    pub app_url: Arc<str>,
    /// Stripe, for the company's subscription.
    pub stripe: Stripe,
    /// `STRIPE_WEBHOOK_SECRET`: signs Stripe's webhooks. Webhooks are
    /// refused when unset.
    pub stripe_webhook_secret: Option<Arc<str>>,
    /// The subscription plans (`STRIPE_BILLING_PLANS` / `STRIPE_PRICE_ID`).
    pub billing_plans: Arc<[BillingPlan]>,
    /// `SIGNATURE_STORAGE_BUCKET`: the private bucket of natively signed
    /// documents (read with the storage credentials).
    pub signature_bucket: Option<Arc<str>>,
    /// Native e-signatures, when enabled (`NATIVE_SIGNATURES_ENABLED=1`).
    pub native_signing: Option<crate::native_signing::NativeSigning>,
    /// How `SignWell`'s webhook proves itself: `SIGNWELL_WEBHOOK_SECRET` (a
    /// `token` it sends) and `SIGNWELL_WEBHOOK_ID` (the key of its event hash).
    pub signwell_webhook: SignWellWebhookKeys,
    /// `SIGNATURE_APP_ORIGIN`: the CRM's origin, for links in signature notices.
    pub signature_app_origin: Arc<str>,
    /// Granite Manager's `RingCentral` app, for companies that press Connect.
    pub ringcentral_app: Option<crate::ringcentral_connect::RingCentralApp>,
    /// DNS for the email deliverability and setup checks (fixed answers in
    /// tests).
    pub dns: crate::deliverability::AppDns,
    /// SES identities for the inbound email checks.
    pub ses: SesIdentities,
    /// `SIGNATURE_SESSION_SECRET`: keys the native signer's CSRF token
    /// (32+ bytes); unset, native signing links answer 503.
    pub signing_session_secret: Option<Arc<str>>,
}

/// Where the SES identity lists come from.
#[derive(Clone)]
pub enum SesIdentities {
    /// The AWS account (`ListEmailIdentities`).
    Aws,
    /// Fixed lists (tests), or `None`: SES could not be asked.
    Fixed(Option<Arc<common::amazon::email::IdentitySets>>),
}

impl SesIdentities {
    pub async fn sets(&self) -> Option<common::amazon::email::IdentitySets> {
        match self {
            Self::Aws => common::amazon::email::identity_sets().await,
            Self::Fixed(sets) => sets.as_deref().cloned(),
        }
    }
}

/// The keys `SignWell`'s webhook calls are checked with; none set: refused.
#[derive(Clone, Default)]
pub struct SignWellWebhookKeys {
    pub secret: Option<Arc<str>>,
    pub id: Option<Arc<str>>,
}

impl SignWellWebhookKeys {
    pub fn new(secret: &str, id: &str) -> Self {
        let set = |v: &str| Some(v.trim()).filter(|v| !v.is_empty()).map(Arc::from);
        Self {
            secret: set(secret),
            id: set(id),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("SESSION_SECRET environment variable must be set")]
pub struct MissingSessionSecret;

impl AppState {
    pub fn new(pool: MySqlPool, session_secret: &str, allowed_origins: &str) -> Self {
        Self {
            pool,
            session_secret: Arc::from(session_secret),
            allowed_origins: parse_origins(allowed_origins).into(),
            storage: Storage::Disabled,
            telegram: TelegramBots::disabled(),
            webhooks: WebhooksClient::disabled(),
            sms_upkeep: Arc::default(),
            sms_sends: Arc::default(),
            flow_starts: Arc::default(),
            compose_contexts: Arc::default(),
            maps: Maps::disabled(),
            call_starts: Arc::default(),
            telephony_http: reqwest::Client::new(),
            cloudtalk_base: Arc::from(common::telephony::cloudtalk::BASE_URL),
            ai: Ai::disabled(),
            signwell_base: Arc::from("https://www.signwell.com/api/v1"),
            transcriber: Transcriber::disabled(),
            qbo: Qbo::disabled(),
            job_key: None,
            contract_templates_base: Arc::from(crate::routes::sales::contract::TEMPLATES_BASE),
            cookie: crate::auth::cookie::CookieConfig::default(),
            app_url: Arc::from("http://localhost:5173"),
            stripe: Stripe::disabled(),
            stripe_webhook_secret: None,
            billing_plans: Arc::from([]),
            signature_bucket: None,
            native_signing: None,
            signwell_webhook: SignWellWebhookKeys::default(),
            signature_app_origin: Arc::from(""),
            ringcentral_app: None,
            dns: crate::deliverability::AppDns::Fixed(Arc::default()),
            ses: SesIdentities::Fixed(None),
            signing_session_secret: None,
        }
    }

    #[must_use]
    pub fn with_signwell_webhook(mut self, keys: SignWellWebhookKeys) -> Self {
        self.signwell_webhook = keys;
        self
    }

    #[must_use]
    pub fn with_signature_app_origin(mut self, origin: &str) -> Self {
        self.signature_app_origin = Arc::from(origin.trim().trim_end_matches('/'));
        self
    }

    #[must_use]
    pub fn with_stripe(mut self, stripe: Stripe) -> Self {
        self.stripe = stripe;
        self
    }

    #[must_use]
    pub fn with_stripe_webhook_secret(mut self, secret: &str) -> Self {
        self.stripe_webhook_secret = Some(secret.trim())
            .filter(|secret| !secret.is_empty())
            .map(Arc::from);
        self
    }

    #[must_use]
    pub fn with_billing_plans(mut self, plans: Vec<BillingPlan>) -> Self {
        self.billing_plans = plans.into();
        self
    }

    #[must_use]
    pub fn with_cookie(mut self, cookie: crate::auth::cookie::CookieConfig) -> Self {
        self.cookie = cookie;
        self
    }

    #[must_use]
    pub fn with_signature_bucket(mut self, bucket: &str) -> Self {
        let bucket = bucket.trim();
        self.signature_bucket = (!bucket.is_empty()).then(|| Arc::from(bucket));
        self.storage = self.storage.keeping_private(bucket);
        self
    }

    #[must_use]
    pub fn with_native_signing(
        mut self,
        signing: Option<crate::native_signing::NativeSigning>,
    ) -> Self {
        self.native_signing = signing;
        self
    }

    #[must_use]
    pub fn with_app_url(mut self, url: &str) -> Self {
        self.app_url = Arc::from(url.trim().trim_end_matches('/'));
        self
    }

    #[must_use]
    pub fn with_storage(mut self, storage: Storage) -> Self {
        self.storage =
            storage.keeping_private(self.signature_bucket.as_deref().unwrap_or_default());
        self
    }

    #[must_use]
    pub fn with_telegram(mut self, telegram: TelegramBots) -> Self {
        self.telegram = telegram;
        self
    }

    /// Ignored unless 32 bytes or longer.
    #[must_use]
    pub fn with_signing_session_secret(mut self, secret: &str) -> Self {
        self.signing_session_secret = (secret.len() >= 32).then(|| Arc::from(secret));
        self
    }

    #[must_use]
    pub fn with_dns(mut self, dns: crate::deliverability::AppDns) -> Self {
        self.dns = dns;
        self
    }

    #[must_use]
    pub fn with_ses(mut self, ses: SesIdentities) -> Self {
        self.ses = ses;
        self
    }

    #[must_use]
    pub fn with_ringcentral_app(
        mut self,
        app: Option<crate::ringcentral_connect::RingCentralApp>,
    ) -> Self {
        self.ringcentral_app = app;
        self
    }

    #[must_use]
    pub fn with_cloudtalk_base(mut self, base: &str) -> Self {
        self.cloudtalk_base = Arc::from(base.trim_end_matches('/'));
        self
    }

    #[must_use]
    pub fn with_signwell_base(mut self, base: &str) -> Self {
        self.signwell_base = Arc::from(base.trim_end_matches('/'));
        self
    }

    #[must_use]
    pub fn with_contract_templates_base(mut self, base: &str) -> Self {
        self.contract_templates_base = Arc::from(base.trim_end_matches('/'));
        self
    }

    #[must_use]
    pub fn with_ai(mut self, ai: Ai) -> Self {
        self.ai = ai;
        self
    }

    #[must_use]
    pub fn with_transcriber(mut self, transcriber: Transcriber) -> Self {
        self.transcriber = transcriber;
        self
    }

    #[must_use]
    pub fn with_qbo(mut self, qbo: Qbo) -> Self {
        self.qbo = qbo;
        self
    }

    #[must_use]
    pub fn with_job_key(mut self, key: &str) -> Self {
        self.job_key = Some(key.trim())
            .filter(|key| !key.is_empty())
            .map(Arc::from);
        self
    }

    #[must_use]
    pub fn with_maps(mut self, maps: Maps) -> Self {
        self.maps = maps;
        self
    }

    #[must_use]
    pub fn with_webhooks(mut self, webhooks: WebhooksClient) -> Self {
        self.webhooks = webhooks;
        self
    }

    pub fn from_env(pool: MySqlPool) -> Result<Self, MissingSessionSecret> {
        let secret = std::env::var("SESSION_SECRET").map_err(|_| MissingSessionSecret)?;
        let origins = std::env::var("API_ALLOWED_ORIGINS")
            .unwrap_or_else(|_| DEFAULT_ALLOWED_ORIGINS.to_string());
        Ok(Self::new(pool, &secret, &origins)
            .with_storage(Storage::from_env())
            .with_telegram(TelegramBots::from_env())
            .with_webhooks(WebhooksClient::from_env())
            .with_maps(Maps::from_env())
            .with_ai(Ai::from_env())
            .with_transcriber(Transcriber::from_env())
            .with_qbo(Qbo::from_env())
            .with_job_key(&std::env::var("LAMBDA_KEY").unwrap_or_default())
            .with_cookie(cookie_config_from_env())
            .with_stripe(Stripe::from_env())
            .with_stripe_webhook_secret(&std::env::var("STRIPE_WEBHOOK_SECRET").unwrap_or_default())
            .with_billing_plans(crate::billing::reactivation::configured_plans())
            .with_signature_bucket(&std::env::var("SIGNATURE_STORAGE_BUCKET").unwrap_or_default())
            .with_native_signing(crate::native_signing::NativeSigning::from_env())
            .with_signwell_webhook(SignWellWebhookKeys::new(
                &std::env::var("SIGNWELL_WEBHOOK_SECRET").unwrap_or_default(),
                &std::env::var("SIGNWELL_WEBHOOK_ID").unwrap_or_default(),
            ))
            .with_signature_app_origin(&std::env::var("SIGNATURE_APP_ORIGIN").unwrap_or_default())
            .with_cloudtalk_base(
                &std::env::var("CLOUDTALK_BASE_URL")
                    .ok()
                    .filter(|base| !base.trim().is_empty())
                    .unwrap_or_else(|| common::telephony::cloudtalk::BASE_URL.to_string()),
            )
            .with_signwell_base(
                &std::env::var("SIGNWELL_BASE_URL")
                    .ok()
                    .filter(|base| !base.trim().is_empty())
                    .unwrap_or_else(|| "https://www.signwell.com/api/v1".to_string()),
            )
            .with_app_url(
                &std::env::var("APP_URL").unwrap_or_else(|_| "https://granite-manager.com".into()),
            )
            .with_ringcentral_app(crate::ringcentral_connect::RingCentralApp::from_env())
            .with_dns(crate::deliverability::AppDns::system())
            .with_ses(SesIdentities::Aws)
            .with_signing_session_secret(
                &std::env::var("SIGNATURE_SESSION_SECRET").unwrap_or_default(),
            ))
    }

    /// Credentialed CORS: the browser sends the `__session` cookie, so the
    /// origin must be echoed back exactly (no `*`).
    /// A browser origin may write: one of the app's, or the signing page's
    /// (`SIGNATURE_APP_ORIGIN`).
    #[must_use]
    pub fn origin_allowed(&self, origin: &str) -> bool {
        origin_allowed(&self.allowed_origins, origin)
            || (!self.signature_app_origin.is_empty() && origin == &*self.signature_app_origin)
    }

    pub fn cors_layer(&self) -> CorsLayer {
        let origins = Arc::clone(&self.allowed_origins);
        let signing = Arc::clone(&self.signature_app_origin);
        CorsLayer::new()
            .allow_origin(AllowOrigin::predicate(move |origin, _| {
                origin.to_str().is_ok_and(|origin| {
                    origin_allowed(&origins, origin) || (!signing.is_empty() && origin == &*signing)
                })
            }))
            .allow_credentials(true)
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::PUT,
                Method::PATCH,
                Method::DELETE,
            ])
            .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION])
    }
}

/// `SESSION_COOKIE_DOMAIN` (default `.granite-manager.com`; `none` for
/// host-only) and `SESSION_COOKIE_SECURE` (default on; `false` for local HTTP).
fn cookie_config_from_env() -> crate::auth::cookie::CookieConfig {
    let domain =
        std::env::var("SESSION_COOKIE_DOMAIN").unwrap_or_else(|_| ".granite-manager.com".into());
    let domain = domain.trim();
    crate::auth::cookie::CookieConfig {
        domain: (!domain.is_empty() && !domain.eq_ignore_ascii_case("none"))
            .then(|| domain.to_string()),
        secure: !std::env::var("SESSION_COOKIE_SECURE")
            .is_ok_and(|value| value.trim().eq_ignore_ascii_case("false")),
    }
}

fn parse_origins(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|origin| !origin.is_empty())
        .map(str::to_string)
        .collect()
}

/// Exact match, or `scheme://*.domain` matching any single-or-multi level subdomain.
pub fn origin_allowed(allowed: &[String], origin: &str) -> bool {
    allowed.iter().any(|pattern| {
        if let Some((scheme, domain)) = pattern.split_once("://*.") {
            origin
                .strip_prefix(scheme)
                .and_then(|rest| rest.strip_prefix("://"))
                .and_then(|host| host.strip_suffix(domain))
                .is_some_and(|sub| sub.len() > 1 && sub.ends_with('.'))
        } else {
            pattern == origin
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{origin_allowed, parse_origins};

    #[test]
    fn wildcard_origins_match_subdomains_only() {
        let allowed = parse_origins("https://*.granite-manager.com, http://localhost:5173");
        assert!(origin_allowed(
            &allowed,
            "https://granitedepotindy.granite-manager.com"
        ));
        assert!(origin_allowed(&allowed, "http://localhost:5173"));
        assert!(!origin_allowed(&allowed, "https://granite-manager.com"));
        assert!(!origin_allowed(&allowed, "https://evilgranite-manager.com"));
        assert!(!origin_allowed(&allowed, "http://x.granite-manager.com"));
        assert!(!origin_allowed(&allowed, "http://localhost:3000"));
    }
}
