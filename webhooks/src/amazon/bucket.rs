use aws_config::BehaviorVersion;
use aws_sdk_s3::Client;
use bytes::Bytes;

pub trait S3Bucket: Send + Sync + Clone {
    fn read_bytes<'a>(
        &'a self,
        bucket: &'a str,
        key: &'a str,
    ) -> impl Future<Output = Result<Bytes, String>> + Send + 'a;

    fn send_file<'a>(
        &'a self,
        bucket: &'a str,
        key: &'a str,
        data: Bytes,
    ) -> impl Future<Output = Result<String, String>> + Send + 'a;
}

#[derive(Clone)]
pub struct CustomClient {}

impl S3Bucket for CustomClient {
    async fn read_bytes(&self, bucket: &str, key: &str) -> Result<Bytes, String> {
        let config = aws_config::load_defaults(BehaviorVersion::latest()).await;
        let client = Client::new(&config);

        let get_object_output = client
            .get_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        let bytes = get_object_output
            .body
            .collect()
            .await
            .map_err(|e| e.to_string())?
            .into_bytes();

        Ok(bytes)
    }

    async fn send_file(&self, bucket: &str, key: &str, data: Bytes) -> Result<String, String> {
        let config = aws_config::load_defaults(BehaviorVersion::latest()).await;
        let client = Client::new(&config);

        client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(data.into())
            .send()
            .await
            .map_err(|e| e.to_string())?;

        Ok(format!("s3://{bucket}/{key}"))
    }
}
