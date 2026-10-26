//! AWS Signature Version 4 for the few AWS calls the API makes itself (S3
//! objects, one SSM parameter), sent with `reqwest`: whole SDK crates for
//! them cost far more build time than the calls are worth.

use aws_credential_types::Credentials;
use aws_sigv4::http_request::{
    PayloadChecksumKind, PercentEncodingMode, SignableBody, SignableRequest, SignatureLocation,
    SigningSettings, UriPathNormalizationMode, sign,
};
use aws_sigv4::sign::v4::SigningParams;
use aws_smithy_runtime_api::client::identity::Identity;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use std::time::{Duration, SystemTime};

/// Everything but AWS's unreserved characters (`A-Z a-z 0-9 - . _ ~`).
pub const AWS_QUERY: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Like [`AWS_QUERY`], keeping the `/` between the parts of an S3 key.
pub const AWS_PATH: &AsciiSet = &AWS_QUERY.remove(b'/');

/// Signs requests to one AWS service in one region.
#[derive(Clone, Debug)]
pub struct Signer {
    identity: Identity,
    region: String,
    service: &'static str,
}

impl Signer {
    pub fn new(credentials: Credentials, region: impl Into<String>, service: &'static str) -> Self {
        Self {
            identity: credentials.into(),
            region: region.into(),
            service,
        }
    }

    pub fn region(&self) -> &str {
        &self.region
    }

    fn settings(&self) -> SigningSettings {
        let mut settings = SigningSettings::default();
        // S3 checks the path as sent and wants the payload hash header.
        if self.service == "s3" {
            settings.percent_encoding_mode = PercentEncodingMode::Single;
            settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
            settings.uri_path_normalization_mode = UriPathNormalizationMode::Disabled;
        }
        settings
    }

    fn instructions(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: SignableBody<'_>,
        settings: SigningSettings,
        time: SystemTime,
    ) -> Result<aws_sigv4::http_request::SigningInstructions, String> {
        let params: aws_sigv4::http_request::SigningParams<'_> = SigningParams::builder()
            .identity(&self.identity)
            .region(&self.region)
            .name(self.service)
            .time(time)
            .settings(settings)
            .build()
            .map_err(|err| err.to_string())?
            .into();
        let request = SignableRequest::new(method, url, headers.iter().copied(), body)
            .map_err(|err| err.to_string())?;
        Ok(sign(request, &params)
            .map_err(|err| err.to_string())?
            .into_parts()
            .0)
    }

    /// The headers that sign this request; send them along with `headers`.
    /// `body` is hashed into the signature; `None` signs `UNSIGNED-PAYLOAD`
    /// (S3 over HTTPS).
    pub fn sign_headers(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<Vec<(String, String)>, String> {
        let body = body.map_or(SignableBody::UnsignedPayload, SignableBody::Bytes);
        let instructions = self.instructions(
            method,
            url,
            headers,
            body,
            self.settings(),
            SystemTime::now(),
        )?;
        Ok(instructions
            .headers()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect())
    }

    /// `url` with a signature in its query that works for `expires_in`. The
    /// request must then carry exactly `headers`.
    pub fn presign(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        expires_in: Duration,
    ) -> Result<String, String> {
        self.presign_at(method, url, headers, expires_in, SystemTime::now())
    }

    fn presign_at(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        expires_in: Duration,
        time: SystemTime,
    ) -> Result<String, String> {
        let mut settings = self.settings();
        settings.signature_location = SignatureLocation::QueryParams;
        settings.expires_in = Some(expires_in);
        settings.payload_checksum_kind = PayloadChecksumKind::NoHeader;
        let instructions = self.instructions(
            method,
            url,
            headers,
            SignableBody::UnsignedPayload,
            settings,
            time,
        )?;
        let mut signed = url.to_string();
        for (name, value) in instructions.params() {
            signed.push(if signed.contains('?') { '&' } else { '?' });
            signed.push_str(name);
            signed.push('=');
            signed.extend(utf8_percent_encode(value, AWS_QUERY));
        }
        Ok(signed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presigned_urls_carry_the_signature_in_the_query() {
        let signer = Signer::new(
            Credentials::new("AKID", "secret", Some("token".into()), None, "test"),
            "us-east-2",
            "s3",
        );
        let url = signer
            .presign_at(
                "GET",
                "https://files.s3.us-east-2.amazonaws.com/a%20b.jpg?response-content-type=image%2Fjpeg",
                &[],
                Duration::from_secs(3600),
                SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            )
            .unwrap();
        assert!(
            url.starts_with(
                "https://files.s3.us-east-2.amazonaws.com/a%20b.jpg?response-content-type=image%2Fjpeg&"
            ),
            "{url}"
        );
        for part in [
            "X-Amz-Algorithm=AWS4-HMAC-SHA256",
            "X-Amz-Credential=AKID%2F20231114%2Fus-east-2%2Fs3%2Faws4_request",
            "X-Amz-Date=20231114T221320Z",
            "X-Amz-Expires=3600",
            "X-Amz-SignedHeaders=host",
            "X-Amz-Security-Token=token",
            "X-Amz-Signature=",
        ] {
            assert!(url.contains(part), "{part} in {url}");
        }
    }
}
