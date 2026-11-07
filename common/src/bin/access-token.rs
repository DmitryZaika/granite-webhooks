//! Makes, lists and revokes webhooks access tokens (`common::access_token`).
//!
//! ```text
//! access-token create --name NAME --scopes leads[,telephony...]
//!                     (--company-id ID | --internal)
//!                     (--expires YYYY-MM-DD | --no-expiry)
//!                     [--created-by WHO]
//! access-token list [--company-id ID]
//! access-token revoke ID
//! ```
//!
//! The database is `ACCESS_TOKEN_DATABASE_URL`, else `DATABASE_URL`. Cargo
//! pins `DATABASE_URL` to the local MySQL (`.cargo/config.toml`), so set
//! `ACCESS_TOKEN_DATABASE_URL` to reach a real one. The token is printed once;
//! only its hash is stored.

use chrono::NaiveDate;
use common::access_token::{self, NewAccessToken, Scope};
use sqlx::MySqlPool;
use std::process::ExitCode;

const USAGE: &str = "\
usage:
  access-token create --name NAME --scopes SCOPE[,SCOPE...]
                      (--company-id ID | --internal)
                      (--expires YYYY-MM-DD | --no-expiry)
                      [--created-by WHO]
  access-token list [--company-id ID]
  access-token revoke ID

scopes: leads, internal, telephony, telegram, ses
database: ACCESS_TOKEN_DATABASE_URL, else DATABASE_URL";

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: &[String]) -> Result<(), String> {
    let (command, rest) = args.split_first().ok_or(USAGE)?;
    match command.as_str() {
        "create" => create(&parse_create(rest)?).await,
        "list" => list(parse_list(rest)?).await,
        "revoke" => revoke(parse_revoke(rest)?).await,
        _ => Err(USAGE.to_string()),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct CreateArgs {
    name: String,
    company_id: Option<i32>,
    scopes: Vec<Scope>,
    expires: Option<NaiveDate>,
    created_by: Option<String>,
}

/// `--flag value` pairs, plus the bare flags `--internal` and `--no-expiry`.
fn flags(args: &[String]) -> Result<Vec<(&str, Option<&str>)>, String> {
    let mut parsed = Vec::new();
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--internal" | "--no-expiry" => parsed.push((flag.as_str(), None)),
            "--name" | "--company-id" | "--scopes" | "--expires" | "--created-by" => {
                let value = iter.next().ok_or(format!("{flag} needs a value"))?;
                parsed.push((flag.as_str(), Some(value.as_str())));
            }
            _ => return Err(format!("unknown argument {flag}\n\n{USAGE}")),
        }
    }
    Ok(parsed)
}

fn parse_company_id(value: &str) -> Result<i32, String> {
    value
        .parse()
        .map_err(|_| format!("--company-id must be a number, got {value}"))
}

fn parse_create(args: &[String]) -> Result<CreateArgs, String> {
    let mut name = None;
    let mut company_id = None;
    let mut internal = false;
    let mut scopes = Vec::new();
    let mut expires = None;
    let mut no_expiry = false;
    let mut created_by = None;
    for (flag, value) in flags(args)? {
        match (flag, value) {
            ("--name", Some(value)) => name = Some(value.trim().to_string()),
            ("--company-id", Some(value)) => company_id = Some(parse_company_id(value)?),
            ("--internal", None) => internal = true,
            ("--scopes", Some(value)) => {
                for part in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    let scope = Scope::parse(part).ok_or(format!("unknown scope {part}"))?;
                    if !scopes.contains(&scope) {
                        scopes.push(scope);
                    }
                }
            }
            ("--expires", Some(value)) => {
                expires = Some(
                    NaiveDate::parse_from_str(value, "%Y-%m-%d")
                        .map_err(|_| format!("--expires must be YYYY-MM-DD, got {value}"))?,
                );
            }
            ("--no-expiry", None) => no_expiry = true,
            ("--created-by", Some(value)) => created_by = Some(value.to_string()),
            _ => unreachable!("flags() only returns known flags"),
        }
    }
    let name = name
        .filter(|name| !name.is_empty())
        .ok_or("--name is required")?;
    if scopes.is_empty() {
        return Err("--scopes is required".to_string());
    }
    // Both choices are explicit: an internal token works for every company,
    // and a token that never expires is never cut off by surprise.
    if company_id.is_some() == internal {
        return Err("give exactly one of --company-id ID or --internal".to_string());
    }
    if expires.is_some() == no_expiry {
        return Err("give exactly one of --expires YYYY-MM-DD or --no-expiry".to_string());
    }
    Ok(CreateArgs {
        name,
        company_id,
        scopes,
        expires,
        created_by,
    })
}

fn parse_list(args: &[String]) -> Result<Option<i32>, String> {
    let mut company_id = None;
    for (flag, value) in flags(args)? {
        match (flag, value) {
            ("--company-id", Some(value)) => company_id = Some(parse_company_id(value)?),
            _ => return Err(format!("list takes only --company-id\n\n{USAGE}")),
        }
    }
    Ok(company_id)
}

fn parse_revoke(args: &[String]) -> Result<i64, String> {
    match args {
        [id] => id
            .parse()
            .map_err(|_| format!("revoke takes a token id, got {id}")),
        _ => Err(USAGE.to_string()),
    }
}

/// The database URL without its password, to show which database is used.
fn redacted(url: &str) -> String {
    match (url.find("://"), url.rfind('@')) {
        (Some(scheme), Some(at)) if at > scheme => {
            format!("{}***{}", &url[..scheme + 3], &url[at..])
        }
        _ => url.to_string(),
    }
}

async fn connect() -> Result<MySqlPool, String> {
    let url = std::env::var("ACCESS_TOKEN_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .map_err(|_| "set ACCESS_TOKEN_DATABASE_URL (or DATABASE_URL)".to_string())?;
    eprintln!("database: {}", redacted(&url));
    MySqlPool::connect(&url)
        .await
        .map_err(|error| format!("cannot connect: {error}"))
}

async fn create(args: &CreateArgs) -> Result<(), String> {
    let pool = connect().await?;
    let (id, token) = access_token::create(
        &pool,
        &NewAccessToken {
            name: &args.name,
            company_id: args.company_id,
            scopes: &args.scopes,
            expires_at: args
                .expires
                .map(|date| date.and_hms_opt(0, 0, 0).expect("midnight exists")),
            created_by: args.created_by.as_deref(),
        },
    )
    .await
    .map_err(|error| format!("cannot create the token: {error}"))?;
    eprintln!(
        "created token {id} ({}). It is shown only this once:",
        args.name
    );
    println!("{token}");
    Ok(())
}

fn date(value: Option<chrono::NaiveDateTime>) -> String {
    value.map_or_else(
        || "-".to_string(),
        |at| at.format("%Y-%m-%d %H:%M").to_string(),
    )
}

async fn list(company_id: Option<i32>) -> Result<(), String> {
    let pool = connect().await?;
    let tokens = access_token::list(&pool, company_id)
        .await
        .map_err(|error| format!("cannot list tokens: {error}"))?;
    println!(
        "{:>5}  {:<32}  {:>7}  {:<24}  {:<6}  {:<16}  {:<16}  {:<16}  {:<16}",
        "id", "name", "company", "scopes", "ends", "created", "expires", "revoked", "last used"
    );
    for token in tokens {
        println!(
            "{:>5}  {:<32}  {:>7}  {:<24}  …{:<5}  {:<16}  {:<16}  {:<16}  {:<16}",
            token.id,
            token.name,
            token
                .company_id
                .map_or_else(|| "all".to_string(), |id| id.to_string()),
            token.scopes,
            token.token_hint,
            date(Some(token.created_at)),
            date(token.expires_at),
            date(token.revoked_at),
            date(token.last_used_at),
        );
    }
    Ok(())
}

async fn revoke(id: i64) -> Result<(), String> {
    let pool = connect().await?;
    let revoked = access_token::revoke(&pool, id)
        .await
        .map_err(|error| format!("cannot revoke: {error}"))?;
    if revoked {
        eprintln!("revoked token {id}; it stops working within a minute");
        Ok(())
    } else {
        Err(format!("no token {id}, or it was already revoked"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn create_needs_explicit_company_and_expiry() {
        let parsed = parse_create(&args(
            "--name Acme --scopes leads,telephony,leads --company-id 12 --expires 2027-10-25",
        ))
        .unwrap();
        assert_eq!(
            parsed,
            CreateArgs {
                name: "Acme".to_string(),
                company_id: Some(12),
                scopes: vec![Scope::Leads, Scope::Telephony],
                expires: NaiveDate::from_ymd_opt(2027, 10, 25),
                created_by: None,
            }
        );

        assert!(parse_create(&args("--name A --scopes leads --no-expiry")).is_err());
        assert!(
            parse_create(&args(
                "--name A --scopes leads --internal --company-id 1 --no-expiry"
            ))
            .is_err()
        );
        assert!(parse_create(&args("--name A --scopes leads --internal")).is_err());
        assert!(
            parse_create(&args(
                "--name A --scopes leads --internal --no-expiry --expires 2027-01-01"
            ))
            .is_err()
        );
        assert!(parse_create(&args("--name A --scopes nope --internal --no-expiry")).is_err());
        assert!(parse_create(&args("--scopes leads --internal --no-expiry")).is_err());
        assert!(parse_create(&args("--name A --internal --no-expiry")).is_err());
        assert!(
            parse_create(&args("--name A --scopes ses --internal --no-expiry"))
                .unwrap()
                .company_id
                .is_none()
        );
    }

    #[test]
    fn list_and_revoke_arguments() {
        assert_eq!(parse_list(&args("")).unwrap(), None);
        assert_eq!(parse_list(&args("--company-id 4")).unwrap(), Some(4));
        assert!(parse_list(&args("--internal")).is_err());
        assert_eq!(parse_revoke(&args("17")).unwrap(), 17);
        assert!(parse_revoke(&args("x")).is_err());
        assert!(parse_revoke(&args("")).is_err());
    }

    #[test]
    fn database_password_is_hidden() {
        assert_eq!(
            redacted("mysql://root:secret@db.example.com:3306/granite"),
            "mysql://***@db.example.com:3306/granite"
        );
        assert_eq!(redacted("mysql://localhost/db"), "mysql://localhost/db");
    }
}
