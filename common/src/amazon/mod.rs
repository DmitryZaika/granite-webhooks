//! The AWS calls the apps make, signed with [`sigv4`] and sent with
//! `reqwest`: SES ([`email`]) and S3 objects ([`s3`]).

pub mod credentials;
pub mod email;
pub mod s3;
pub mod sigv4;

use std::sync::LazyLock;
use std::time::Duration;

/// Tries per call, as in the SDK's standard retry mode.
const MAX_ATTEMPTS: u32 = 3;

static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    crate::http::client_builder()
        .connect_timeout(Duration::from_secs(5))
        .build()
        .expect("HTTP client")
});

/// The region of the Lambda (`AWS_REGION`), us-east-2 elsewhere.
pub fn region() -> String {
    ["AWS_REGION", "AWS_DEFAULT_REGION"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| "us-east-2".to_string())
}

/// Sends a request signed by `signer`, its body hashed into the signature
/// when `sign_body` (else `UNSIGNED-PAYLOAD`, for S3). Like the SDK, it is
/// sent again, after a short random wait, when it could not be sent or AWS
/// answered that it is busy (429, 500, 502, 503, 504).
async fn send(
    signer: &sigv4::Signer,
    method: reqwest::Method,
    url: &str,
    headers: &[(&str, &str)],
    body: bytes::Bytes,
    sign_body: bool,
) -> Result<reqwest::Response, String> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let signed = signer.sign_headers(
            method.as_str(),
            url,
            headers,
            sign_body.then_some(body.as_ref()),
        )?;
        let mut request = CLIENT.request(method.clone(), url);
        for (name, value) in headers.iter().copied().chain(
            signed
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        ) {
            request = request.header(name, value);
        }
        let sent = request.body(body.clone()).send().await;
        let busy = match &sent {
            Ok(response) => matches!(response.status().as_u16(), 429 | 500 | 502 | 503 | 504),
            Err(_) => true,
        };
        if !busy || attempt == MAX_ATTEMPTS {
            return sent.map_err(|error| error.without_url().to_string());
        }
        let ceiling = 100u64 << attempt;
        tokio::time::sleep(Duration::from_millis(rand::random_range(0..ceiling))).await;
    }
}
