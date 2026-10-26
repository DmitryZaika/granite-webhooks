//! Phone pushes of the lead alerts sent to Telegram. The API sends them
//! (`POST /v1/phone/pushes/lead`): the phones and FCM are there, not here.

use lambda_http::tracing;
use reqwest::{Client, RequestBuilder};
use serde::Serialize;
use std::time::Duration;

/// The whole call may take this long. It is awaited, not spawned: the Lambda
/// freezes once it answers, so a spawned call might never run.
const PUSH_TIMEOUT: Duration = Duration::from_secs(3);

/// What a lead push is about (the API's `LeadKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LeadKind {
    /// A new lead, for the managers to assign.
    New,
    /// A lead that came in again.
    Repeat,
    /// A lead a manager assigned to this rep.
    Assigned,
}

/// Who is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audience {
    /// The company's sales managers (linked to Telegram or not).
    Managers,
    /// This user: the lead's sales rep.
    Rep(i32),
}

#[derive(Debug, Serialize)]
struct LeadPush {
    company_id: i32,
    customer_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    deal_id: Option<u64>,
    kind: LeadKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_id: Option<i32>,
}

/// API root and the key to send.
#[cfg(not(any(test, feature = "test-support")))]
fn api_endpoint() -> Option<(String, String)> {
    Some((
        std::env::var("API_URL").ok()?,
        std::env::var("LAMBDA_KEY").ok()?,
    ))
}

/// Tests get the loopback stub they installed (and no call at all
/// otherwise), never the real API.
#[cfg(any(test, feature = "test-support"))]
fn api_endpoint() -> Option<(String, String)> {
    crate::tests::stub_http::api_base_url().map(|base| (base, "test-key".to_string()))
}

/// `POST {api_url}/v1/phone/pushes/lead`.
fn push_request(client: &Client, api_url: &str, key: &str, push: &LeadPush) -> RequestBuilder {
    client
        .post(format!(
            "{}/v1/phone/pushes/lead",
            api_url.trim_end_matches('/')
        ))
        .header("Authorization", key)
        .timeout(PUSH_TIMEOUT)
        .json(push)
}

/// Asks the API to push lead news about customer `customer_id` (deal
/// `deal_id`, else the customer's newest) to `audience`'s phones. Waits
/// [`PUSH_TIMEOUT`] at most; a failure is only logged, the lead is already
/// saved.
pub async fn push_lead(
    company_id: i32,
    customer_id: i64,
    deal_id: Option<u64>,
    kind: LeadKind,
    audience: Audience,
) {
    let Some((api_url, key)) = api_endpoint() else {
        #[cfg(not(any(test, feature = "test-support")))]
        tracing::warn!(customer_id, "API_URL or LAMBDA_KEY not set; no lead push");
        return;
    };
    let push = LeadPush {
        company_id,
        customer_id,
        deal_id,
        kind,
        user_id: match audience {
            Audience::Managers => None,
            Audience::Rep(user_id) => Some(user_id),
        },
    };
    match push_request(&common::http::client(), &api_url, &key, &push)
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => {}
        Ok(response) => tracing::warn!(
            customer_id,
            status = response.status().as_u16(),
            "Lead push refused"
        ),
        Err(error) => tracing::warn!(
            customer_id,
            error = %error.without_url(),
            "Lead push not sent"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::stub_http::{StubServer, use_api};
    use serde_json::json;

    #[test]
    fn the_request_names_the_lead_and_carries_the_key() {
        let push = LeadPush {
            company_id: 3,
            customer_id: 42,
            deal_id: None,
            kind: LeadKind::New,
            user_id: None,
        };
        let request = push_request(
            &common::http::client(),
            "https://api.test/",
            "lambda-key",
            &push,
        )
        .build()
        .unwrap();
        assert_eq!(request.method(), "POST");
        assert_eq!(
            request.url().as_str(),
            "https://api.test/v1/phone/pushes/lead"
        );
        assert_eq!(request.headers()["authorization"], "lambda-key");
        assert_eq!(request.timeout(), Some(&PUSH_TIMEOUT));
        let body: serde_json::Value =
            serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(
            body,
            json!({ "company_id": 3, "customer_id": 42, "kind": "new" })
        );
    }

    #[tokio::test]
    async fn a_push_to_the_rep_reaches_the_api() {
        let stub = StubServer::start(|_| Some(json!({ "pushed": 1 }))).await;
        use_api(&stub.base);
        push_lead(3, 42, Some(7), LeadKind::Assigned, Audience::Rep(9)).await;
        let requests = stub.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, "/v1/phone/pushes/lead");
        let body: serde_json::Value = serde_json::from_str(&requests[0].body).unwrap();
        assert_eq!(
            body,
            json!({ "company_id": 3, "customer_id": 42, "deal_id": 7, "kind": "assigned", "user_id": 9 })
        );
    }

    #[tokio::test]
    async fn a_failed_push_is_only_logged() {
        let stub = StubServer::start(|_| None).await;
        use_api(&stub.base);
        push_lead(3, 42, None, LeadKind::Repeat, Audience::Managers).await;
        assert_eq!(stub.requests().len(), 1);
        use_api("http://127.0.0.1:9");
        push_lead(3, 42, None, LeadKind::Repeat, Audience::Managers).await;
    }
}
