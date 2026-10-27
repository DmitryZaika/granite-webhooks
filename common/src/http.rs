//! HTTP clients for the apps' dependency graph, which has no bundled TLS
//! crypto provider: rustls' default (aws-lc-rs) is a long C build, so the
//! clients use ring.

use std::sync::Once;
use std::time::Duration;

/// Makes ring the process-wide rustls provider (once). reqwest is built with
/// `rustls-no-provider` and panics on a client built before this ran.
pub fn install_crypto_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Err: another provider was installed first, which works as well.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// How long an unused connection is kept for the next request. Short: a
/// Lambda is frozen between invocations, and a connection the server closed
/// meanwhile fails the next request instead of a fresh one being opened.
const POOL_IDLE: Duration = Duration::from_secs(20);

/// `common::http::client_builder()` with the crypto provider installed.
#[allow(clippy::disallowed_methods)]
pub fn client_builder() -> reqwest::ClientBuilder {
    install_crypto_provider();
    reqwest::Client::builder().pool_idle_timeout(POOL_IDLE)
}

/// `common::http::client()` with the crypto provider installed.
pub fn client() -> reqwest::Client {
    client_builder().build().expect("HTTP client")
}
