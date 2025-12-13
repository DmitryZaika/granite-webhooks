use aws_sdk_s3::Client;
use bytes::Bytes;

pub trait S3Bucket {
    async fn read_bytes(&self, bucket: &str, key: &str) -> Result<Bytes, String>;
}

pub struct CustomClient {}

impl S3Bucket for CustomClient {
    async fn read_bytes(&self, bucket: &str, key: &str) -> Result<Bytes, String> {
        let config = aws_config::load_from_env().await;
        let client = Client::new(&config);
        let get_object_output = match client.get_object().bucket(bucket).key(key).send().await {
            Ok(output) => output,
            Err(error) => return Err(error.to_string()),
        };

        let email_bytes = match get_object_output.body.collect().await {
            Ok(bytes) => bytes.into_bytes(),
            Err(error) => return Err(error.to_string()),
        };
        Ok(email_bytes)
    }
}
