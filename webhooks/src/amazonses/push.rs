//! Phone pushes for inbound email. The API sends them
//! (`POST /v1/emails/inbound-push`): the phones and FCM are there, not here.

use lambda_http::tracing;
use reqwest::{Client, RequestBuilder};
use serde::Serialize;
use std::time::Duration;

/// The whole call may take this long. It is awaited, not spawned: the Lambda
/// freezes once it answers, so a spawned call might never run.
const PUSH_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Serialize)]
struct InboundPush {
    email_id: u64,
}

/// API root and the key to send.
#[cfg(not(test))]
fn api_endpoint() -> Option<(String, String)> {
    Some((
        std::env::var("API_URL").ok()?,
        std::env::var("LAMBDA_KEY").ok()?,
    ))
}

/// Tests get the loopback stub they installed (and no call at all
/// otherwise), never the real API.
#[cfg(test)]
fn api_endpoint() -> Option<(String, String)> {
    crate::tests::stub_http::api_base_url().map(|base| (base, "test-key".to_string()))
}

/// `POST {api_url}/v1/emails/inbound-push` for stored email `email_id`.
fn push_request(client: &Client, api_url: &str, key: &str, email_id: u64) -> RequestBuilder {
    client
        .post(format!(
            "{}/v1/emails/inbound-push",
            api_url.trim_end_matches('/')
        ))
        .header("Authorization", key)
        .timeout(PUSH_TIMEOUT)
        .json(&InboundPush { email_id })
}

/// Asks the API to push the newly stored inbound email `email_id` to its
/// recipients' phones. Waits [`PUSH_TIMEOUT`] at most; a failure is only
/// logged, the email is already stored.
pub async fn push_inbound_email(email_id: u64) {
    let Some((api_url, key)) = api_endpoint() else {
        #[cfg(not(test))]
        tracing::warn!(email_id, "API_URL or LAMBDA_KEY not set; no email push");
        return;
    };
    let sent = push_request(&Client::new(), &api_url, &key, email_id)
        .send()
        .await;
    match sent {
        Ok(response) if response.status().is_success() => {}
        Ok(response) => tracing::warn!(
            email_id,
            status = response.status().as_u16(),
            "Inbound email push refused"
        ),
        Err(error) => tracing::warn!(
            email_id,
            error = %error.without_url(),
            "Inbound email push not sent"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::stub_http::{StubServer, use_api};
    use serde_json::json;

    #[test]
    fn the_request_names_the_email_and_carries_the_key() {
        let request = push_request(&Client::new(), "https://api.test/", "lambda-key", 42)
            .build()
            .unwrap();
        assert_eq!(request.method(), "POST");
        assert_eq!(
            request.url().as_str(),
            "https://api.test/v1/emails/inbound-push"
        );
        assert_eq!(request.headers()["authorization"], "lambda-key");
        assert_eq!(request.headers()["content-type"], "application/json");
        assert_eq!(request.timeout(), Some(&PUSH_TIMEOUT));
        let body: serde_json::Value =
            serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body, json!({ "email_id": 42 }));
    }

    #[tokio::test]
    async fn a_push_reaches_the_api() {
        let stub = StubServer::start(|_| Some(json!({ "pushed": 1 }))).await;
        use_api(&stub.base);
        push_inbound_email(7).await;
        let requests = stub.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].path, "/v1/emails/inbound-push");
        assert_eq!(requests[0].body, r#"{"email_id":7}"#);
    }

    #[tokio::test]
    async fn a_failed_push_is_only_logged() {
        // The stub answers 404; then nothing listens at all.
        let stub = StubServer::start(|_| None).await;
        use_api(&stub.base);
        push_inbound_email(7).await;
        assert_eq!(stub.requests().len(), 1);
        use_api("http://127.0.0.1:9");
        push_inbound_email(7).await;
    }
}
