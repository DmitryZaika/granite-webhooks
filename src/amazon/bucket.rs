use aws_config::BehaviorVersion;
use aws_sdk_s3::Client;
use bytes::Bytes;

pub trait S3Bucket: Send + Sync {
    fn read_bytes<'a>(
        &'a self,
        bucket: &'a str,
        key: &'a str,
    ) -> impl Future<Output = Result<Bytes, String>> + Send + 'a;
}

pub struct CustomClient {}

impl S3Bucket for CustomClient {
    fn read_bytes<'a>(
        &'a self,
        bucket: &'a str,
        key: &'a str,
    ) -> impl Future<Output = Result<Bytes, String>> + Send + 'a {
        async move {
            let config = aws_config::load_defaults(BehaviorVersion::latest()).await;
            let client = Client::new(&config);

            let get_object_output = client
                .get_object()
                .bucket(bucket)
                .key(key)
                .send()
                .await
                .map_err(|e| e.to_string())?;

            let email_bytes = get_object_output
                .body
                .collect()
                .await
                .map_err(|e| e.to_string())?
                .into_bytes();

            Ok(email_bytes)
        }
    }
}
