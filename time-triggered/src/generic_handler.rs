use lambda_runtime::{tracing, Error, LambdaEvent};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Deserialize, Debug)]
pub(crate) struct EventBridgeEvent {
    pub account: String,
    pub detail: Value,
    #[serde(rename = "detail-type")]
    pub detail_type: String,
    pub id: String,
    pub region: String,
    pub resources: Vec<String>,
    pub source: String,
    pub time: String,
    pub version: String,
}

#[derive(Serialize)]
pub(crate) struct OutgoingMessage {
    req_id: String,
    msg: String,
}

/// This is the main body for the function.
/// Write your code inside it.
/// There are some code example in the following URLs:
/// - https://github.com/awslabs/aws-lambda-rust-runtime/tree/main/examples
/// - https://github.com/aws-samples/serverless-rust-demo/
pub(crate) async fn function_handler(
    event: LambdaEvent<EventBridgeEvent>,
) -> Result<OutgoingMessage, Error> {
    // This will now print the full JSON structure to your CloudWatch logs
    tracing::info!("Received event: {:?}", event.payload);

    let resp = OutgoingMessage {
        req_id: event.context.request_id,
        msg: "Check CloudWatch logs for the payload structure.".to_string(),
    };

    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lambda_runtime::{Context, LambdaEvent};

    #[tokio::test]
    async fn test_generic_handler() {
        // Mocking the data we saw in the logs
        let incoming = EventBridgeEvent {
            account: "123456789012".to_string(),
            detail: serde_json::json!({}),
            detail_type: "Scheduled Event".to_string(),
            id: "uuid-1234".to_string(),
            region: "us-east-2".to_string(),
            resources: vec!["arn:aws:scheduler...".to_string()],
            source: "aws.scheduler".to_string(),
            time: "2026-04-19T16:04:00Z".to_string(),
            version: "0".to_string(),
        };

        let event = LambdaEvent::new(incoming, Context::default());
        let response = function_handler(event).await.unwrap();

        // Adjusting expectation to match the actual fields
        assert!(response.msg.contains("Check"));
    }
}
