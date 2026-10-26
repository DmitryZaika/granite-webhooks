//! S3 objects (`GetObject`, `PutObject`) in the Lambda's region.

use super::sigv4::{AWS_PATH, Signer};
use bytes::Bytes;
use percent_encoding::utf8_percent_encode;

/// The bytes of object `key` in `bucket`. An error names S3's error code
/// (`NoSuchKey: The specified key does not exist.`).
pub async fn get_object(bucket: &str, key: &str) -> Result<Bytes, String> {
    let response = call("GetObject", reqwest::Method::GET, bucket, key, Bytes::new()).await?;
    response
        .bytes()
        .await
        .map_err(|error| format!("S3 GetObject: {}", error.without_url()))
}

/// Stores `body` as object `key` of `bucket`.
pub async fn put_object(bucket: &str, key: &str, body: Bytes) -> Result<(), String> {
    call("PutObject", reqwest::Method::PUT, bucket, key, body)
        .await
        .map(drop)
}

async fn call(
    operation: &str,
    method: reqwest::Method,
    bucket: &str,
    key: &str,
    body: Bytes,
) -> Result<reqwest::Response, String> {
    let credentials = super::credentials::credentials().await?;
    let region = super::region();
    let url = object_url(bucket, key, &region);
    let signer = Signer::new(credentials, region, "s3");
    let response = super::send(&signer, method, &url, &[], body, false)
        .await
        .map_err(|error| format!("S3 {operation}: {error}"))?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let xml = response.text().await.unwrap_or_default();
    Err(format!(
        "S3 {operation} {}: {}: {}",
        status.as_u16(),
        xml_text(&xml, "Code").unwrap_or_default(),
        xml_text(&xml, "Message").unwrap_or_default()
    ))
}

/// Virtual-hosted style, or path style for a bucket with a dot in its name
/// (the HTTPS certificate covers one label before `.s3.`), as the SDK does.
fn object_url(bucket: &str, key: &str, region: &str) -> String {
    let key = utf8_percent_encode(key, AWS_PATH);
    if bucket.contains('.') {
        format!("https://s3.{region}.amazonaws.com/{bucket}/{key}")
    } else {
        format!("https://{bucket}.s3.{region}.amazonaws.com/{key}")
    }
}

/// The text of the first `<tag>` in an S3 XML document.
fn xml_text<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let start = xml.find(&format!("<{tag}>"))? + tag.len() + 2;
    let end = start + xml[start..].find(&format!("</{tag}>"))?;
    Some(xml[start..end].trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_urls_encode_the_key_and_keep_its_slashes() {
        assert_eq!(
            object_url("gd-email-attachments", "inbound/a b+c.pdf", "us-east-2"),
            "https://gd-email-attachments.s3.us-east-2.amazonaws.com/inbound/a%20b%2Bc.pdf"
        );
        assert_eq!(
            object_url("mail.example.com", "x", "us-east-2"),
            "https://s3.us-east-2.amazonaws.com/mail.example.com/x"
        );
    }

    #[test]
    fn xml_errors_are_read() {
        let xml = "<?xml version=\"1.0\"?><Error><Code>NoSuchKey</Code>\
                   <Message>The specified key does not exist.</Message></Error>";
        assert_eq!(xml_text(xml, "Code"), Some("NoSuchKey"));
        assert_eq!(
            xml_text(xml, "Message"),
            Some("The specified key does not exist.")
        );
        assert_eq!(xml_text(xml, "RequestId"), None);
    }
}
