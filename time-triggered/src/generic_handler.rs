use lambda_runtime::{tracing, Error, LambdaEvent};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
pub(crate) struct IncomingMessage {
    command: String,
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
pub(crate) async fn function_handler(event: LambdaEvent<String>) -> Result<OutgoingMessage, Error> {
    tracing::info!("{:?}", event);
    // Extract some useful info from the request

    // Prepare the outgoing message
    let resp = OutgoingMessage {
        req_id: event.context.request_id,
        msg: format!("Command ."),
    };

    // Return `OutgoingMessage` (it will be serialized to JSON automatically by the runtime)
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lambda_runtime::{Context, LambdaEvent};

    #[tokio::test]
    async fn test_generic_handler() {
        let event = LambdaEvent::new("hello".to_string(), Context::default());
        let response = function_handler(event).await.unwrap();
        assert_eq!(response.msg, "Command test.");
    }
}
