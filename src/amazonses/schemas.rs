use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct SesEvent {
    pub version: String,
    pub id: String,

    #[serde(rename = "detail-type")]
    pub detail_type: String,

    pub source: String,
    pub account: String,
    pub time: String,
    pub region: String,
    pub resources: Vec<String>,
    pub detail: Detail,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Detail {
    #[serde(rename = "eventType")]
    pub event_type: String,
    pub mail: Mail,
    pub open: Open,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Mail {
    pub timestamp: String,
    pub source: String,
    #[serde(rename = "sendingAccountId")]
    pub sending_account_id: String,
    #[serde(rename = "messageId")]
    pub message_id: String,
    pub destination: Vec<String>,
    #[serde(rename = "headersTruncated")]
    pub headers_truncated: bool,
    pub headers: Vec<Header>,
    #[serde(rename = "commonHeaders")]
    pub common_headers: CommonHeaders,
    pub tags: Tags,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Header {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CommonHeaders {
    pub from: Vec<String>,
    pub to: Vec<String>,
    #[serde(rename = "messageId")]
    pub message_id: String,
    pub subject: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Tags {
    #[serde(rename = "ses:source-tls-version")]
    pub ses_source_tls_version: Vec<String>,

    #[serde(rename = "ses:operation")]
    pub ses_operation: Vec<String>,

    #[serde(rename = "ses:configuration-set")]
    pub ses_configuration_set: Vec<String>,

    #[serde(rename = "ses:source-ip")]
    pub ses_source_ip: Vec<String>,

    #[serde(rename = "ses:from-domain")]
    pub ses_from_domain: Vec<String>,

    #[serde(rename = "ses:caller-identity")]
    pub ses_caller_identity: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Open {
    pub timestamp: String,
    #[serde(rename = "userAgent")]
    pub user_agent: String,
    #[serde(rename = "ipAddress")]
    pub ip_address: String,
}
