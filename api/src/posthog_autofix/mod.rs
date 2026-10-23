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
use tokio::sync::OnceCell;

/// The auto-fix settings and the HTTP client for PostHog and GitHub.
///
/// The settings are read on the first auto-fix request, not at start-up, so
/// a slow or unreachable Parameter Store never delays the rest of the api.
#[derive(Clone)]
pub struct Autofix {
    settings: Arc<OnceCell<Arc<Config>>>,
    pub http: reqwest::Client,
}

/// Parameter Store gets this long; past it the request runs unconfigured and
/// the next one tries again.
const PARAMETER_TIMEOUT: Duration = Duration::from_secs(3);

impl Autofix {
    /// With fixed settings (tests, local runs).
    pub fn new(config: Config) -> Self {
        Self::with_settings(OnceCell::new_with(Some(Arc::new(config))))
    }

    /// Settings read from the environment and Parameter Store when first
    /// needed.
    pub fn from_env() -> Self {
        Self::with_settings(OnceCell::new())
    }

    /// Mode off, nothing configured.
    pub fn disabled() -> Self {
        Self::new(Config::default())
    }

    fn with_settings(settings: OnceCell<Arc<Config>>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent("granite-manager-posthog-autofix")
            .build()
            .unwrap_or_default();
        Self {
            settings: Arc::new(settings),
            http,
        }
    }

    /// The settings, read once per Lambda instance. A read that failed is not
    /// kept, so the next request tries Parameter Store again.
    pub async fn config(&self) -> Arc<Config> {
        if let Some(config) = self.settings.get() {
            return Arc::clone(config);
        }
        match load().await {
            Ok(config) => Arc::clone(
                self.settings
                    .get_or_init(|| async { Arc::new(config) })
                    .await,
            ),
            Err(partial) => Arc::new(*partial),
        }
    }
}

/// The environment, then the Parameter Store parameter for whatever it leaves
/// unset. Mode off reads nothing. `Err` holds the environment's settings
/// alone, when the parameter could not be read.
async fn load() -> Result<Config, Box<Config>> {
    let env = |name: &str| std::env::var(name).ok();
    if Config::from_lookup(env).mode == Mode::Off {
        return Ok(Config::from_lookup(env));
    }
    let name = env("POSTHOG_AUTOFIX_PARAMETER")
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| parameter::DEFAULT_NAME.to_string());
    let read = tokio::time::timeout(PARAMETER_TIMEOUT, parameter::fetch(&name))
        .await
        .unwrap_or_else(|_| Err("timed out".into()));
    match read {
        Ok(text) => Ok(Config::from_lookup(parameter::lookup(
            env,
            &parameter::parse(&text),
        ))),
        Err(error) => {
            lambda_http::tracing::error!(
                error,
                parameter = name,
                "PostHog auto-fix settings not read; retried on the next request"
            );
            Err(Box::new(Config::from_lookup(env)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fixed_settings_are_served_without_reading_anything() {
        let autofix = Autofix::new(Config {
            max_per_day: 9,
            ..Config::default()
        });
        assert_eq!(autofix.config().await.max_per_day, 9);
        assert!(Arc::ptr_eq(
            &autofix.config().await,
            &autofix.config().await
        ));
        assert_eq!(Autofix::disabled().config().await.mode, Mode::Off);
    }
}
