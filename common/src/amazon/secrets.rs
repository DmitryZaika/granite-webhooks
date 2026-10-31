//! The Lambdas' keys, tokens and passwords, kept in AWS Secrets Manager
//! instead of the Lambda's environment, where anyone who can view the
//! function's configuration reads them.
//!
//! `SECRETS_ID` names one secret whose value is a JSON object of
//! `{"NAME": "value"}`. At cold start, before anything reads the environment,
//! [`load_into_env`] copies each pair into the process environment, so the
//! code that reads `std::env::var` is unchanged. A variable already set in the
//! environment wins. Without `SECRETS_ID` (a laptop with a `.env`) it does
//! nothing.

use crate::amazon::sigv4::Signer;
use std::collections::BTreeMap;

/// Names the secret (its name or ARN) to load.
pub const ID_VAR: &str = "SECRETS_ID";

/// Copies the secret named by `SECRETS_ID` into the process environment.
/// Call it first thing in `main`: an error stops the Lambda rather than run
/// it without its keys. The values are never logged; the names are.
pub async fn load_into_env() -> Result<(), String> {
    let Some(id) = std::env::var(ID_VAR)
        .ok()
        .filter(|id| !id.trim().is_empty())
    else {
        return Ok(());
    };
    let pairs = parse(&fetch(&id).await?)?;
    let mut loaded = Vec::new();
    for (name, value) in pairs {
        if std::env::var_os(&name).is_some_and(|set| !set.is_empty()) {
            continue;
        }
        // SAFETY: called at the start of `main`, before any task reads or
        // writes the environment.
        unsafe { std::env::set_var(&name, value) };
        loaded.push(name);
    }
    tracing::info!(secret = %id, names = ?loaded, "loaded secrets into the environment");
    Ok(())
}

/// The secret's JSON object; every value must be a string.
fn parse(text: &str) -> Result<BTreeMap<String, String>, String> {
    serde_json::from_str(text)
        .map_err(|error| format!("the secret is not a JSON object of strings: {error}"))
}

/// The `SecretString` of `id` (Secrets Manager `GetSecretValue`, in the
/// Lambda's region).
async fn fetch(id: &str) -> Result<String, String> {
    let region = super::region();
    let credentials = super::credentials::credentials().await?;
    let url = format!("https://secretsmanager.{region}.amazonaws.com/");
    let body = serde_json::json!({ "SecretId": id }).to_string();
    let headers = [
        ("content-type", "application/x-amz-json-1.1"),
        ("x-amz-target", "secretsmanager.GetSecretValue"),
    ];
    let signer = Signer::new(credentials, region, "secretsmanager");
    let response = super::send(
        &signer,
        reqwest::Method::POST,
        &url,
        &headers,
        bytes::Bytes::from(body),
        true,
    )
    .await
    .map_err(|error| format!("Secrets Manager GetSecretValue {id}: {error}"))?;
    let status = response.status();
    let reply: serde_json::Value = response
        .json()
        .await
        .map_err(|error| format!("Secrets Manager GetSecretValue {id}: {error}"))?;
    if !status.is_success() {
        let kind = reply["__type"].as_str().unwrap_or_default();
        let message = reply["message"]
            .as_str()
            .or_else(|| reply["Message"].as_str())
            .unwrap_or_default();
        return Err(format!(
            "Secrets Manager GetSecretValue {id} {status}: {kind} {message}"
        ));
    }
    reply["SecretString"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("the secret {id} has no SecretString"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_secret_is_an_object_of_strings() {
        let pairs = parse(r#"{"DATABASE_URL": "mysql://u:p@h/db", "AES_KEY": "abc"}"#).unwrap();
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs["DATABASE_URL"], "mysql://u:p@h/db");

        assert!(parse(r#"{"PORT": 3306}"#).is_err());
        assert!(parse("DATABASE_URL=x").is_err());
    }
}
