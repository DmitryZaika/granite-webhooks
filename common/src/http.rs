//! HTTP clients for the API's dependency graph, which has no bundled TLS
//! crypto provider: rustls' default (aws-lc-rs) is a long C build, so the
//! clients use ring, which the API already links.

use std::sync::{Once, OnceLock};

/// Makes ring the process-wide rustls provider (once). reqwest is built with
/// `rustls-no-provider` and panics on a client built before this ran.
pub fn install_crypto_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Err: another provider was installed first, which works as well.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// `common::http::client_builder()` with the crypto provider installed.
#[allow(clippy::disallowed_methods)]
pub fn client_builder() -> reqwest::ClientBuilder {
    install_crypto_provider();
    reqwest::Client::builder()
}

/// `common::http::client()` with the crypto provider installed.
pub fn client() -> reqwest::Client {
    client_builder().build().expect("HTTP client")
}

/// The HTTPS client for AWS SDK clients (rustls on ring, not the SDK's
/// default aws-lc-rs).
pub fn aws_http_client() -> aws_smithy_runtime_api::client::http::SharedHttpClient {
    use aws_smithy_http_client::tls::{Provider, rustls_provider::CryptoMode};
    static CLIENT: OnceLock<aws_smithy_runtime_api::client::http::SharedHttpClient> =
        OnceLock::new();
    CLIENT
        .get_or_init(|| {
            aws_smithy_http_client::Builder::new()
                .tls_provider(Provider::Rustls(CryptoMode::Ring))
                .build_https()
        })
        .clone()
}

/// AWS configuration from the environment in `region`, with
/// [`aws_http_client`].
pub async fn aws_config(
    region: impl Into<std::borrow::Cow<'static, str>>,
) -> aws_config::SdkConfig {
    aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(region))
        .http_client(aws_http_client())
        .load()
        .await
}
