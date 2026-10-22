//! PostHog auto-fix: each new, unique production error becomes one fix run.
//!
//! The intake job (`POST /v1/jobs/posthog-autofix`, every minute) reads new
//! issues from PostHog into the `posthog_autofix_runs` ledger, lets them
//! settle, folds duplicates by [`signature`], skips noise, and starts the
//! GitHub workflow `posthog-autofix.yml` within the caps. The workflow's
//! agent reproduces the error as a failing test, fixes it, and the workflow
//! proves red/green before it opens a pull request. It reports back through
//! `POST /v1/posthog-autofix/runs/{id}/started` and `.../result`, and the
//! developers are emailed.
//!
//! Pure decisions live in [`signature`] and [`policy`]; PostHog, GitHub and
//! email sit behind the [`posthog::IssueSource`], [`github::Dispatcher`] and
//! [`notify::Mailer`] traits, so [`intake::tick`] runs against fakes in tests.

pub mod callback;
pub mod config;
pub mod emails;
pub mod github;
pub mod intake;
pub mod ledger;
pub mod model;
pub mod notify;
pub mod parameter;
pub mod policy;
pub mod posthog;
pub mod signature;
pub mod writeback;

pub use config::{Config, Mode};

use std::sync::Arc;
use std::time::Duration;

/// The auto-fix settings and the HTTP client for PostHog and GitHub.
#[derive(Clone)]
pub struct Autofix {
    pub config: Arc<Config>,
    pub http: reqwest::Client,
}

impl Autofix {
    pub fn new(config: Config) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent("granite-manager-posthog-autofix")
            .build()
            .unwrap_or_default();
        Self {
            config: Arc::new(config),
            http,
        }
    }

    pub fn from_env() -> Self {
        Self::new(Config::from_env())
    }

    /// The Lambda's settings: the environment, then the Parameter Store
    /// parameter for whatever it leaves unset. Mode off fetches nothing; a
    /// parameter that cannot be read leaves the auto-fix unconfigured.
    pub async fn load() -> Self {
        let env = |name: &str| std::env::var(name).ok();
        if Config::from_lookup(env).mode == Mode::Off {
            return Self::from_env();
        }
        let name = env("POSTHOG_AUTOFIX_PARAMETER")
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| parameter::DEFAULT_NAME.to_string());
        let stored = match parameter::fetch(&name).await {
            Ok(text) => parameter::parse(&text),
            Err(error) => {
                lambda_http::tracing::error!(
                    error,
                    parameter = name,
                    "PostHog auto-fix settings not read; it stays unconfigured"
                );
                std::collections::HashMap::new()
            }
        };
        Self::new(Config::from_lookup(parameter::lookup(env, &stored)))
    }

    /// Mode off, nothing configured.
    pub fn disabled() -> Self {
        Self::new(Config::default())
    }
}
