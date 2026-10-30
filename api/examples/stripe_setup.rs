//! Sets up the Stripe account for self-serve billing: the five prices, the
//! customer portal configuration and the webhook endpoint.
//!
//! cargo run -q -p api --example stripe_setup -- [--apply [--live]] [--webhook-url <url>] [--secret-file <path>]
//!
//! Reads STRIPE_SECRET_KEY from the environment only. Without --apply it only
//! reads and reports. --apply with a live key (sk_live_/rk_live_) also needs
//! --live, so production is never written by accident. Creating the webhook
//! endpoint needs --secret-file: Stripe shows the signing secret once, and it
//! is written there with mode 0600 (a failed run removes the empty file).
//! STRIPE_API_BASE (a fake Stripe) is honoured for test keys only.
use api::billing::setup::{KeyMode, key_mode};
use api::billing::stripe::Stripe;
use api::billing::stripe_setup::{Options, run};
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str =
    "usage: stripe_setup [--apply [--live]] [--webhook-url <url>] [--secret-file <path>]";

fn parse(args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut apply = false;
    let mut live = false;
    let mut webhook_url = None;
    let mut secret_file = None;
    let mut args = args;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--apply" => apply = true,
            "--live" => live = true,
            "--webhook-url" => {
                webhook_url = Some(args.next().ok_or("--webhook-url needs a value")?)
            }
            "--secret-file" => {
                secret_file = Some(PathBuf::from(
                    args.next().ok_or("--secret-file needs a value")?,
                ));
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Options {
        live,
        ..Options::from_env(apply, webhook_url, secret_file)
    })
}

#[tokio::main]
async fn main() -> ExitCode {
    let options = match parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{error}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let key = std::env::var("STRIPE_SECRET_KEY").unwrap_or_default();
    let Some(mode) = key_mode(&key, &["sk_", "rk_"]) else {
        eprintln!("STRIPE_SECRET_KEY is not set or is not an sk_/rk_ key; refusing to run");
        return ExitCode::from(2);
    };
    let base = std::env::var("STRIPE_API_BASE")
        .ok()
        .filter(|base| !base.trim().is_empty());
    let stripe = match (base, mode) {
        (Some(base), KeyMode::Test) => Stripe::new(Some(&key), &base),
        (Some(_), KeyMode::Live) => {
            eprintln!("STRIPE_API_BASE is only honoured for test keys; refusing to run");
            return ExitCode::from(2);
        }
        (None, _) => Stripe::new(Some(&key), "https://api.stripe.com"),
    };
    match run(&stripe, mode, &options, &mut std::io::stdout()).await {
        Ok(outcome) if outcome.problems == 0 => ExitCode::SUCCESS,
        Ok(_) => ExitCode::from(1),
        Err(error) => {
            eprintln!("\nstripe_setup stopped: {error}");
            ExitCode::from(2)
        }
    }
}
