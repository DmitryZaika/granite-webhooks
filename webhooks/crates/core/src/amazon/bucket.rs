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
        common::amazon::s3::get_object(bucket, key).await
    }

    async fn send_file(&self, bucket: &str, key: &str, data: Bytes) -> Result<String, String> {
        common::amazon::s3::put_object(bucket, key, data).await?;
        Ok(format!("s3://{bucket}/{key}"))
    }
}
