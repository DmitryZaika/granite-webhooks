//! The Stripe setup script's logic (`billing::stripe_setup`) against the fake Stripe.

use crate::fake_stripe::{self, FakeStripe, Shared};
use api::billing::setup::{KeyMode, REQUIRED_EVENTS};
use api::billing::stripe::API_VERSION;
use api::billing::stripe_setup::{Options, Outcome, SetupError, run};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

const URL: &str = "https://api.example.com/v1/webhooks/stripe";

fn secret_path() -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    std::env::temp_dir().join(format!(
        "stripe-setup-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn options(apply: bool, secret_file: Option<PathBuf>) -> Options {
    Options {
        apply,
        live: false,
        webhook_url: URL.into(),
        secret_file,
        payments_app_url: "https://payments.example.com".into(),
        app_url: "https://example.com".into(),
    }
}

fn empty_account() -> Shared {
    std::sync::Arc::new(std::sync::Mutex::new(FakeStripe::default()))
}

async fn go(shared: &Shared, options: &Options) -> (Result<Outcome, SetupError>, String) {
    let stripe = fake_stripe::client(shared).await;
    let mut out = Vec::new();
    let result = run(&stripe, KeyMode::Test, options, &mut out).await;
    (result, String::from_utf8(out).unwrap())
}

fn writes(shared: &Shared) -> usize {
    shared
        .lock()
        .unwrap()
        .requests
        .iter()
        .filter(|seen| seen.method == "POST")
        .count()
}

#[tokio::test]
async fn a_dry_run_on_an_empty_account_creates_nothing_and_lists_everything() {
    let shared = empty_account();
    let (result, output) = go(&shared, &options(false, None)).await;
    let outcome = result.unwrap();
    assert_eq!(writes(&shared), 0);
    assert_eq!(outcome.changes, 7, "five prices, the portal, the webhook");
    assert_eq!(outcome.problems, 0);
    assert!(output.starts_with("Stripe key mode: TEST\nStripe livemode: false\n"));
    for key in [
        "granite_base_monthly_v1 300.00 USD",
        "granite_extra_user_monthly_v1 30.00 USD",
        "addon_signatures_monthly_v1 15.00 USD",
        "addon_quotes_monthly_v1 75.00 USD",
        "addon_quickbooks_monthly_v1 30.00 USD",
        "granite-manager-no-cancel-v1",
    ] {
        assert!(
            output.contains(&format!("would create  {key}")) || output.contains(key),
            "{key}\n{output}"
        );
    }
    assert!(output.contains("DRY RUN"));
    assert!(output.contains("would create"));
    assert!(output.contains("Dry run: 7 change(s) would be made"));
    println!("{output}");
}

#[tokio::test]
async fn apply_creates_everything_and_keeps_the_secret_out_of_the_output() {
    let shared = empty_account();
    let path = secret_path();
    let (result, output) = go(&shared, &options(true, Some(path.clone()))).await;
    let outcome = result.unwrap();
    assert_eq!(outcome.changes, 7);
    assert_eq!(outcome.problems, 0);

    let fake = shared.lock().unwrap();
    assert_eq!(fake.prices.len(), 5);
    for price in &fake.prices {
        assert_eq!(price["tax_behavior"], "exclusive");
        assert_eq!(price["currency"], "usd");
        assert_eq!(price["recurring"]["interval"], "month");
    }
    let base = fake
        .prices
        .iter()
        .find(|price| price["lookup_key"] == "granite_base_monthly_v1")
        .unwrap();
    assert_eq!(base["unit_amount"], 30_000);
    assert_eq!(base["metadata"]["kind"], "base");
    let quotes = fake
        .prices
        .iter()
        .find(|price| price["lookup_key"] == "addon_quotes_monthly_v1")
        .unwrap();
    assert_eq!(quotes["metadata"]["addon_key"], "quotes");
    for seen in fake.requests.iter().filter(|seen| seen.method == "POST") {
        assert!(seen.idempotency_key.is_some(), "{}", seen.path);
    }

    let portal = &fake.portal_configurations[0];
    assert_eq!(portal["metadata"]["app"], "granite-manager-no-cancel-v1");
    assert_eq!(
        portal["default_return_url"],
        "https://payments.example.com/account"
    );
    assert_eq!(portal["features"]["subscription_cancel"]["enabled"], false);
    assert_eq!(portal["features"]["subscription_update"]["enabled"], false);
    assert_eq!(portal["features"]["invoice_history"]["enabled"], true);
    assert_eq!(
        portal["features"]["customer_update"]["allowed_updates"],
        json!(["name", "email", "address", "phone", "tax_id"])
    );

    let endpoint = &fake.webhook_endpoints[0];
    assert_eq!(endpoint["url"], URL);
    assert_eq!(endpoint["api_version"], API_VERSION);
    assert_eq!(endpoint["enabled_events"], json!(REQUIRED_EVENTS));

    let secret = std::fs::read_to_string(&path).unwrap();
    let secret = secret.trim();
    assert!(secret.starts_with("whsec_"));
    assert!(!output.contains(secret), "{output}");
    assert!(output.contains(&format!("whsec_…{}", &secret[secret.len() - 4..])));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn a_second_apply_changes_nothing() {
    let shared = empty_account();
    let path = secret_path();
    go(&shared, &options(true, Some(path.clone())))
        .await
        .0
        .unwrap();
    std::fs::remove_file(&path).unwrap();
    let before = writes(&shared);

    let (result, output) = go(&shared, &options(true, None)).await;
    let outcome = result.unwrap();
    assert_eq!(outcome.changes, 0, "{output}");
    assert_eq!(outcome.problems, 0, "{output}");
    assert_eq!(writes(&shared), before);
    assert!(!path.exists());
    assert!(output.contains("Stripe shows a signing secret only when the endpoint is created"));
    assert_eq!(outcome.price_ids.len(), 5);
    assert!(outcome.webhook_endpoint_id.is_some());
}

#[tokio::test]
async fn a_price_with_the_wrong_tax_behavior_is_reported_and_left_alone() {
    let shared = FakeStripe::with_catalog();
    {
        let mut fake = shared.lock().unwrap();
        fake.prices[0]["tax_behavior"] = json!("unspecified");
    }
    let path = secret_path();
    let (result, output) = go(&shared, &options(true, Some(path.clone()))).await;
    let outcome = result.unwrap();
    assert_eq!(outcome.problems, 1, "{output}");
    assert!(output.contains("tax_behavior is unspecified, expected exclusive"));
    assert!(output.contains("granite_base_monthly_v1_v2"));
    let fake = shared.lock().unwrap();
    assert!(
        fake.calls("POST", "/v1/prices").is_empty(),
        "no price is created or changed"
    );
    assert_eq!(fake.prices[0]["tax_behavior"], "unspecified");
    drop(fake);
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn a_wrong_amount_is_reported_loudly() {
    let shared = FakeStripe::with_catalog();
    shared.lock().unwrap().prices[1]["unit_amount"] = json!(3_500);
    let (result, output) = go(&shared, &options(false, None)).await;
    assert_eq!(result.unwrap().problems, 1);
    assert!(output.contains("amount is 3500 cents, expected 3000"));
}

#[tokio::test]
async fn creating_the_endpoint_without_a_secret_file_is_refused_before_any_write() {
    let shared = empty_account();
    let (result, _) = go(&shared, &options(true, None)).await;
    let Err(SetupError::Refused(reason)) = result else {
        panic!("expected a refusal");
    };
    assert!(reason.contains("--secret-file"), "{reason}");
    assert_eq!(writes(&shared), 0);
}

#[tokio::test]
async fn an_existing_secret_file_is_never_overwritten() {
    let shared = empty_account();
    let path = secret_path();
    std::fs::write(&path, "keep me").unwrap();
    let (result, _) = go(&shared, &options(true, Some(path.clone()))).await;
    assert!(matches!(result, Err(SetupError::Refused(_))));
    assert_eq!(writes(&shared), 0);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep me");
    std::fs::remove_file(path).unwrap();
}

fn endpoint(events: &[&str], version: &str) -> Value {
    json!({ "id": "we_old", "url": URL, "status": "enabled",
        "enabled_events": events, "api_version": version })
}

#[tokio::test]
async fn an_endpoint_with_other_events_gets_exactly_the_required_ones() {
    let shared = FakeStripe::with_catalog();
    shared
        .lock()
        .unwrap()
        .webhook_endpoints
        .push(endpoint(&["invoice.paid", "charge.succeeded"], API_VERSION));
    let (result, output) = go(&shared, &options(true, None)).await;
    let outcome = result.unwrap();
    assert!(
        output.contains("extra events: charge.succeeded"),
        "{output}"
    );
    assert!(output.contains("missing events:"));
    let fake = shared.lock().unwrap();
    assert_eq!(
        fake.webhook_endpoints[0]["enabled_events"],
        json!(REQUIRED_EVENTS)
    );
    assert_eq!(fake.calls("POST", "/v1/webhook_endpoints").len(), 0);
    assert_eq!(fake.calls("POST", "/v1/webhook_endpoints/we_old").len(), 1);
    assert!(outcome.secret_file_written.is_none());
}

#[tokio::test]
async fn a_wrong_endpoint_api_version_is_reported_not_changed() {
    let shared = FakeStripe::with_catalog();
    shared
        .lock()
        .unwrap()
        .webhook_endpoints
        .push(endpoint(&["invoice.paid"], "2020-01-01"));
    let (result, output) = go(&shared, &options(true, None)).await;
    assert_eq!(result.unwrap().problems, 1);
    assert!(output.contains("Stripe cannot change it on an existing endpoint"));
    let fake = shared.lock().unwrap();
    assert!(
        fake.calls("POST", "/v1/webhook_endpoints/we_old")
            .is_empty()
    );
    assert_eq!(fake.webhook_endpoints[0]["api_version"], "2020-01-01");
}

#[tokio::test]
async fn a_portal_configuration_that_allows_cancelling_is_updated() {
    let shared = FakeStripe::with_catalog();
    let path = secret_path();
    go(&shared, &options(true, Some(path.clone())))
        .await
        .0
        .unwrap();
    std::fs::remove_file(&path).unwrap();
    shared.lock().unwrap().portal_configurations[0]["features"]["subscription_cancel"]["enabled"] =
        json!(true);

    let (dry, output) = go(&shared, &options(false, None)).await;
    assert_eq!(dry.unwrap().changes, 1);
    assert!(
        output.contains("subscription_cancel is on, expected off"),
        "{output}"
    );
    assert_eq!(
        shared.lock().unwrap().portal_configurations[0]["features"]["subscription_cancel"]["enabled"],
        true
    );

    let (result, _) = go(&shared, &options(true, None)).await;
    assert_eq!(result.unwrap().changes, 1);
    let fake = shared.lock().unwrap();
    assert_eq!(
        fake.portal_configurations[0]["features"]["subscription_cancel"]["enabled"],
        false
    );
    assert_eq!(fake.portal_configurations.len(), 1);
}

#[tokio::test]
async fn a_key_and_a_stripe_account_in_different_modes_stop_the_run() {
    let shared = empty_account();
    let stripe = fake_stripe::client(&shared).await;
    let mut out = Vec::new();
    let live = Options {
        live: true,
        ..options(true, None)
    };
    let result = run(&stripe, KeyMode::Live, &live, &mut out).await;
    let Err(SetupError::Refused(reason)) = result else {
        panic!("expected a refusal");
    };
    assert!(reason.contains("livemode disagree"), "{reason}");
    assert_eq!(writes(&shared), 0);
}

#[tokio::test]
async fn applying_with_a_live_key_needs_the_live_flag() {
    let shared = empty_account();
    let stripe = fake_stripe::client(&shared).await;
    let mut out = Vec::new();
    let result = run(&stripe, KeyMode::Live, &options(true, None), &mut out).await;
    let Err(SetupError::Refused(reason)) = result else {
        panic!("expected a refusal");
    };
    assert!(reason.contains("--live"), "{reason}");
    assert!(
        shared.lock().unwrap().requests.is_empty(),
        "refused before reading Stripe"
    );
    // A dry run with a live key needs no flag (it stops here only because
    // the fake is a test-mode account).
    let result = run(&stripe, KeyMode::Live, &options(false, None), &mut out).await;
    let Err(SetupError::Refused(reason)) = result else {
        panic!("expected the livemode refusal");
    };
    assert!(reason.contains("livemode disagree"), "{reason}");
}

#[tokio::test]
async fn a_failed_run_leaves_no_secret_file_behind() {
    let shared = empty_account();
    shared.lock().unwrap().fail(
        axum::http::Method::POST,
        "/v1/webhook_endpoints",
        fake_stripe::FailMode::Before,
        1,
    );
    let path = secret_path();
    let (result, _) = go(&shared, &options(true, Some(path.clone()))).await;
    assert!(matches!(result, Err(SetupError::Stripe(_))), "{result:?}");
    assert!(!path.exists(), "the reserved file is removed");

    // The next run with the same path creates the endpoint and keeps its secret.
    let (result, _) = go(&shared, &options(true, Some(path.clone()))).await;
    assert_eq!(result.unwrap().secret_file_written, Some(path.clone()));
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .starts_with("whsec_")
    );
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn an_empty_secret_file_from_an_old_run_is_reused() {
    let shared = empty_account();
    let path = secret_path();
    std::fs::write(&path, "").unwrap();
    let (result, _) = go(&shared, &options(true, Some(path.clone()))).await;
    assert_eq!(result.unwrap().secret_file_written, Some(path.clone()));
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .starts_with("whsec_")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    std::fs::remove_file(path).unwrap();
}

/// An archived price holding the base lookup key.
fn archive_base(shared: &Shared, cents: i64, product: &str) {
    let mut fake = shared.lock().unwrap();
    let mut archived = fake_stripe::price("price_old_base", "granite_base_monthly_v1", cents);
    archived["active"] = json!(false);
    archived["product"] = json!({ "id": "prod_old", "name": product });
    fake.prices.push(archived);
}

#[tokio::test]
async fn an_archived_price_of_ours_hands_its_lookup_key_to_the_new_one() {
    let shared = empty_account();
    archive_base(&shared, 30_000, "Granite Manager Base (up to 10 users)");
    let (dry, output) = go(&shared, &options(false, None)).await;
    assert_eq!(dry.unwrap().problems, 0, "{output}");
    assert!(
        output.contains("lookup key moved from archived price price_old_base"),
        "{output}"
    );

    let path = secret_path();
    let (result, output) = go(&shared, &options(true, Some(path.clone()))).await;
    let outcome = result.unwrap();
    assert_eq!(outcome.problems, 0, "{output}");
    let fake = shared.lock().unwrap();
    let transfers: Vec<_> = fake
        .calls("POST", "/v1/prices")
        .into_iter()
        .filter(|seen| seen.field("transfer_lookup_key").as_deref() == Some("true"))
        .collect();
    assert_eq!(transfers.len(), 1);
    assert_eq!(
        transfers[0].field("lookup_key").as_deref(),
        Some("granite_base_monthly_v1")
    );
    let holder: Vec<&Value> = fake
        .prices
        .iter()
        .filter(|price| price["lookup_key"] == "granite_base_monthly_v1")
        .collect();
    assert_eq!(holder.len(), 1);
    assert_eq!(holder[0]["active"], true);
    drop(fake);
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn an_archived_price_that_is_not_ours_stops_that_price() {
    let shared = empty_account();
    archive_base(&shared, 25_000, "Something else");
    let (dry, output) = go(&shared, &options(false, None)).await;
    assert_eq!(dry.unwrap().problems, 1, "{output}");
    assert!(
        output.contains(
            "held by archived price price_old_base (25000 cents, product Something else)"
        ),
        "{output}"
    );

    let path = secret_path();
    let (result, output) = go(&shared, &options(true, Some(path.clone()))).await;
    assert_eq!(result.unwrap().problems, 1, "{output}");
    let fake = shared.lock().unwrap();
    assert!(
        fake.calls("POST", "/v1/prices")
            .iter()
            .all(|seen| seen.field("lookup_key").as_deref() != Some("granite_base_monthly_v1")),
        "the base price is not created"
    );
    assert_eq!(fake.calls("POST", "/v1/prices").len(), 4);
    drop(fake);
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn the_environment_list_reports_automatic_tax_and_stripe_tax() {
    let shared = FakeStripe::with_catalog();
    shared.lock().unwrap().tax_settings = Some(json!({ "status": "active" }));
    let (_, output) = go(&shared, &options(false, None)).await;
    assert!(
        output.contains("STRIPE_AUTOMATIC_TAX: 1 to compute sales tax"),
        "{output}"
    );
    assert!(
        output.contains("Stripe Tax is active on this account"),
        "{output}"
    );
}
