use axum::http::{StatusCode, Uri};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

use serde::Serialize;

#[derive(Serialize, Debug)]
pub struct PostHogEvent {
    pub api_key: String,
    pub event: String,       // "$exception" и т.п.
    pub distinct_id: String, // ВЕРХНИЙ УРОВЕНЬ
    #[serde(skip_serializing_if = "Option::is_none")]
    pub properties: Option<Properties>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>, // опускаем, если None
}

#[derive(Serialize, Debug, Default)]
pub struct Properties {
    #[serde(rename = "$exception_list", skip_serializing_if = "Option::is_none")]
    pub exception_list: Option<Vec<ExceptionItem>>,
    #[serde(rename = "$exception_fingerprint")]
    pub exception_fingerprint: String,

    pub status: Option<u16>,
    pub path: Option<String>,

    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Serialize, Debug)]
pub struct ExceptionItem {
    #[serde(rename = "type")]
    pub exception_type: String,
    #[serde(rename = "value")]
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stacktrace: Option<StackTrace>,
}

#[derive(Serialize, Debug)]
pub struct StackTrace {
    #[serde(rename = "type")]
    pub kind: String, // "raw" | "resolved"
    pub frames: Vec<Frame>,
}

#[derive(Serialize, Debug)]
pub struct Frame {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lineno: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub colno: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_app: Option<bool>,
}

impl PostHogEvent {
    pub fn new_http_exception(
        api_key: impl Into<String>,
        value: impl Into<String>,
        status: StatusCode,
        uri: &Uri,
    ) -> Self {
        let item = ExceptionItem {
            exception_type: "HTTPError".into(),
            value: value.into(),
            stacktrace: Some(StackTrace {
                kind: "raw".into(),
                frames: vec![],
            }),
        };

        let mut hasher = Sha256::new();
        hasher.update(format!("HTTPError|{}|{}", status.as_u16(), uri));
        let fingerprint = format!("{:x}", hasher.finalize());
        Self {
            api_key: api_key.into(),
            event: "$exception".into(),
            distinct_id: "server-webhooks".into(),
            properties: Some(Properties {
                exception_list: Some(vec![item]),
                exception_fingerprint: fingerprint,
                status: Some(status.as_u16()),
                path: Some(uri.to_string()),
                extra: HashMap::new(),
            }),
            timestamp: None,
        }
    }
    pub fn new_general_exception(
        api_key: impl Into<String>,
        value: impl Into<String> + std::fmt::Display + Clone,
        title: impl Into<String> + std::fmt::Display + Clone,
    ) -> Self {
        let item = ExceptionItem {
            exception_type: title.clone().into(),
            value: value.clone().into(),
            stacktrace: Some(StackTrace {
                kind: "raw".into(),
                frames: vec![],
            }),
        };

        let mut hasher = Sha256::new();
        hasher.update(format!("{value}|{title}|"));
        let fingerprint = format!("{:x}", hasher.finalize());
        Self {
            api_key: api_key.into(),
            event: "$exception".into(),
            distinct_id: "server-webhooks".into(),
            properties: Some(Properties {
                exception_list: Some(vec![item]),
                exception_fingerprint: fingerprint,
                status: None,
                path: None,
                extra: HashMap::new(),
            }),
            timestamp: None,
        }
    }
}
