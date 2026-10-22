//! The auto-fix settings stored in AWS Systems Manager Parameter Store.
//!
//! The `granite-api` Lambda's environment is at its 4 KB limit, so only
//! `POSTHOG_AUTOFIX_MODE` and `POSTHOG_AUTOFIX_START_AT` live there. The keys,
//! tokens and recipients are `NAME=value` lines in one SecureString
//! parameter, read once at cold start; a variable set in the environment wins.

use aws_config::Region;
use aws_config::meta::region::RegionProviderChain;
use std::collections::HashMap;
use std::hash::BuildHasher;

/// `POSTHOG_AUTOFIX_PARAMETER` overrides it.
pub const DEFAULT_NAME: &str = "/granite-manager/posthog-autofix";

/// `NAME=value` lines; blank lines and `#` comments are skipped.
pub fn parse(text: &str) -> HashMap<String, String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        .filter(|(name, value)| !name.is_empty() && !value.is_empty())
        .collect()
}

/// A setting from the environment, else from the parameter.
pub fn lookup<S: BuildHasher>(
    env: impl Fn(&str) -> Option<String>,
    stored: &HashMap<String, String, S>,
) -> impl Fn(&str) -> Option<String> {
    move |name| {
        env(name)
            .filter(|value| !value.trim().is_empty())
            .or_else(|| stored.get(name).cloned())
    }
}

/// The decrypted value of the SecureString parameter `name`.
pub async fn fetch(name: &str) -> Result<String, String> {
    let region = RegionProviderChain::default_provider().or_else(Region::new("us-east-2"));
    let config = aws_config::from_env().region(region).load().await;
    let output = aws_sdk_ssm::Client::new(&config)
        .get_parameter()
        .name(name)
        .with_decryption(true)
        .send()
        .await
        .map_err(|error| aws_sdk_ssm::error::DisplayErrorContext(&error).to_string())?;
    output
        .parameter()
        .and_then(|parameter| parameter.value())
        .map(str::to_string)
        .ok_or_else(|| "the parameter has no value".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_parse_like_an_env_file() {
        let stored = parse(
            "# auto-fix\nPOSTHOG_AUTOFIX_POSTHOG_KEY = phx_a \n\nBROKEN\nPOSTHOG_AUTOFIX_RECIPIENTS=a@x.com,b@y.com\nEMPTY=\n",
        );
        assert_eq!(stored.len(), 2);
        assert_eq!(stored["POSTHOG_AUTOFIX_POSTHOG_KEY"], "phx_a");
        assert_eq!(stored["POSTHOG_AUTOFIX_RECIPIENTS"], "a@x.com,b@y.com");
    }

    #[test]
    fn the_environment_wins_over_the_parameter() {
        let stored = parse("POSTHOG_AUTOFIX_MODE=live\nPOSTHOG_AUTOFIX_POSTHOG_KEY=phx_a");
        let env = |name: &str| (name == "POSTHOG_AUTOFIX_MODE").then(|| "dry_run".to_string());
        let read = lookup(env, &stored);
        assert_eq!(read("POSTHOG_AUTOFIX_MODE").as_deref(), Some("dry_run"));
        assert_eq!(
            read("POSTHOG_AUTOFIX_POSTHOG_KEY").as_deref(),
            Some("phx_a")
        );
        assert_eq!(read("POSTHOG_AUTOFIX_GITHUB_TOKEN"), None);
    }
}
