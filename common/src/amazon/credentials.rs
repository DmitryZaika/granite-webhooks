//! AWS credentials for the signed calls (`sigv4`), in place of the SDK's
//! provider chain: a Lambda has its role's credentials in the environment,
//! and on a laptop the AWS CLI hands out its profile's, whatever its kind
//! (keys, SSO, `aws login`).

use aws_credential_types::Credentials;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

/// The CLI's credentials are asked for again this long before they expire.
const REFRESH_BEFORE_EXPIRY: Duration = Duration::from_mins(5);

static CLI_CREDENTIALS: Mutex<Option<Credentials>> = Mutex::new(None);

/// `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and `AWS_SESSION_TOKEN` when
/// they are set (a Lambda), else what `aws configure export-credentials`
/// prints for `AWS_PROFILE` (or the CLI's default profile).
pub async fn credentials() -> Result<Credentials, String> {
    if let Some(credentials) = from_env(|name| std::env::var(name).ok()) {
        return Ok(credentials);
    }
    if let Some(credentials) = cached() {
        return Ok(credentials);
    }
    let credentials = tokio::task::spawn_blocking(from_cli)
        .await
        .map_err(|error| error.to_string())??;
    if let Ok(mut cached) = CLI_CREDENTIALS.lock() {
        *cached = Some(credentials.clone());
    }
    Ok(credentials)
}

fn from_env(var: impl Fn(&str) -> Option<String>) -> Option<Credentials> {
    let set = |name| var(name).filter(|value| !value.trim().is_empty());
    Some(Credentials::new(
        set("AWS_ACCESS_KEY_ID")?,
        set("AWS_SECRET_ACCESS_KEY")?,
        set("AWS_SESSION_TOKEN"),
        None,
        "environment",
    ))
}

fn cached() -> Option<Credentials> {
    let credentials = CLI_CREDENTIALS.lock().ok()?.clone()?;
    let fresh = credentials
        .expiry()
        .is_none_or(|expiry| expiry > SystemTime::now() + REFRESH_BEFORE_EXPIRY);
    fresh.then_some(credentials)
}

fn from_cli() -> Result<Credentials, String> {
    let output = std::process::Command::new("aws")
        .args(["configure", "export-credentials", "--format", "process"])
        .output()
        .map_err(|error| {
            format!("no AWS credentials in the environment, and the AWS CLI did not run: {error}")
        })?;
    if !output.status.success() {
        return Err(format!(
            "aws configure export-credentials: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    parse_process_output(&output.stdout)
}

/// The JSON of a `credential_process` (what `--format process` prints).
fn parse_process_output(json: &[u8]) -> Result<Credentials, String> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Process {
        access_key_id: String,
        secret_access_key: String,
        session_token: Option<String>,
        expiration: Option<String>,
    }
    let process: Process = serde_json::from_slice(json)
        .map_err(|error| format!("aws configure export-credentials printed {error}"))?;
    let expiry = process
        .expiration
        .as_deref()
        .and_then(|expiration| chrono::DateTime::parse_from_rfc3339(expiration).ok())
        .map(SystemTime::from);
    Ok(Credentials::new(
        process.access_key_id,
        process.secret_access_key,
        process.session_token,
        expiry,
        "aws-cli",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_needs_a_key_and_a_secret() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                vars.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_string())
            }
        };
        let lambda = from_env(env(&[
            ("AWS_ACCESS_KEY_ID", "ASIA1"),
            ("AWS_SECRET_ACCESS_KEY", "secret"),
            ("AWS_SESSION_TOKEN", "token"),
        ]))
        .unwrap();
        assert_eq!(lambda.access_key_id(), "ASIA1");
        assert_eq!(lambda.secret_access_key(), "secret");
        assert_eq!(lambda.session_token(), Some("token"));

        let keys = from_env(env(&[
            ("AWS_ACCESS_KEY_ID", "AKIA1"),
            ("AWS_SECRET_ACCESS_KEY", "secret"),
            ("AWS_SESSION_TOKEN", " "),
        ]))
        .unwrap();
        assert_eq!(keys.session_token(), None);

        assert!(from_env(env(&[("AWS_ACCESS_KEY_ID", "AKIA1")])).is_none());
        assert!(
            from_env(env(&[
                ("AWS_ACCESS_KEY_ID", ""),
                ("AWS_SECRET_ACCESS_KEY", "s")
            ]))
            .is_none()
        );
    }

    #[test]
    fn the_cli_output_is_read_with_its_expiry() {
        let credentials = parse_process_output(
            br#"{"Version": 1, "AccessKeyId": "ASIA2", "SecretAccessKey": "s",
                 "SessionToken": "t", "Expiration": "2026-10-26T15:00:00+00:00"}"#,
        )
        .unwrap();
        assert_eq!(credentials.access_key_id(), "ASIA2");
        assert_eq!(credentials.session_token(), Some("t"));
        assert_eq!(
            credentials.expiry(),
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_793_026_800))
        );

        let keys = parse_process_output(
            br#"{"Version": 1, "AccessKeyId": "AKIA3", "SecretAccessKey": "s"}"#,
        )
        .unwrap();
        assert_eq!(keys.session_token(), None);
        assert_eq!(keys.expiry(), None);

        assert!(parse_process_output(b"Unable to locate credentials").is_err());
    }
}
