//! Setting a company up on Telnyx ("Connect Telnyx"): its billing group,
//! outbound voice profile, TeXML application, credential connection and
//! messaging profile; the push credentials the app's phones ring through;
//! and local numbers to search and buy. Names must be unique in the Telnyx
//! account (applications and connections share one namespace): a taken name
//! answers [`TelnyxError::Status`] with code [`NAME_TAKEN`].

use super::{Telnyx, TelnyxError, segment};
use reqwest::Method;
use serde_json::{Value, json};

/// Telnyx's error code for a name already in use.
pub const NAME_TAKEN: &str = "10015";

/// Where the company's calls and texts may go.
const DESTINATIONS: [&str; 2] = ["US", "CA"];

/// The aliases of the Granite Phone app's push credentials (Firebase for
/// Android, `APNs` for iPhone) in the Telnyx account.
pub const ANDROID_PUSH_ALIAS: &str = "granite-phone-android";
pub const IOS_PUSH_ALIAS: &str = "granite-phone-ios";

/// Words that opt a customer out of texts, on top of Telnyx's own
/// (STOP, UNSUBSCRIBE, ...), and the answer they get.
const OPT_OUT_KEYWORDS: [&str; 11] = [
    "STOP",
    "STOPALL",
    "STOP ALL",
    "UNSUBSCRIBE",
    "CANCEL",
    "END",
    "QUIT",
    "REVOKE",
    "OPTOUT",
    "OPT OUT",
    "OPT-OUT",
];
const OPT_OUT_REPLY: &str =
    "You are unsubscribed and will get no more texts from this number. Reply START to resubscribe.";

/// A company's outbound voice profile: US and Canada only, with a daily
/// spend limit (dollars, like `"25.00"`).
pub struct VoiceProfileSpec<'a> {
    pub name: &'a str,
    pub billing_group_id: Option<&'a str>,
    pub daily_spend_limit: &'a str,
}

/// A company's TeXML application: where Telnyx asks what to do with calls
/// to its numbers.
pub struct TexmlAppSpec<'a> {
    pub name: &'a str,
    /// `{webhookBase}/inbound`.
    pub voice_url: &'a str,
    /// `{webhookBase}/call-status`.
    pub status_callback: &'a str,
    /// Where calls go when our webhook fails.
    pub fallback_url: Option<&'a str>,
    pub outbound_voice_profile_id: &'a str,
}

/// A company's credential connection: its phones' SIP credentials live on
/// it, and their outgoing calls come to `webhook_url` parked.
pub struct CredentialConnectionSpec<'a> {
    pub name: &'a str,
    /// `{webhookBase}/outbound`.
    pub webhook_url: &'a str,
    pub outbound_voice_profile_id: &'a str,
    pub android_push_credential_id: Option<&'a str>,
    pub ios_push_credential_id: Option<&'a str>,
}

/// A company's messaging profile: its numbers text through it.
pub struct MessagingProfileSpec<'a> {
    pub name: &'a str,
    /// `{webhookBase}/messaging`.
    pub webhook_url: &'a str,
    pub daily_spend_limit: &'a str,
}

/// The app's push credentials found in the Telnyx account.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushCredentials {
    pub android: Option<String>,
    pub ios: Option<String>,
}

/// A number Telnyx can sell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableNumber {
    /// E.164.
    pub e164: String,
    /// The town (Telnyx's locality or rate center), when given.
    pub locality: Option<String>,
    /// The state, when given.
    pub region: Option<String>,
    /// Dollars a month and up front, like `"1.00000"`.
    pub monthly_cost: Option<String>,
    pub upfront_cost: Option<String>,
    pub currency: Option<String>,
}

/// A number order: `pending` until Telnyx finishes, then `success` or
/// `failure`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumberOrder {
    pub id: String,
    pub status: String,
}

/// The `data.id` of an answer (some Telnyx ids are numbers).
fn data_id(answer: &Value, what: &'static str) -> Result<String, TelnyxError> {
    match &answer["data"]["id"] {
        Value::String(id) if !id.is_empty() => Ok(id.clone()),
        Value::Number(id) => Ok(id.to_string()),
        _ => Err(TelnyxError::Unexpected(what)),
    }
}

fn texml_app_body(spec: &TexmlAppSpec<'_>) -> Value {
    json!({
        "friendly_name": spec.name,
        "voice_url": spec.voice_url,
        "voice_method": "post",
        "status_callback": spec.status_callback,
        "status_callback_method": "post",
        "voice_fallback_url": spec.fallback_url,
        "inbound": { "shaken_stir_enabled": true },
        "outbound": { "outbound_voice_profile_id": spec.outbound_voice_profile_id },
    })
}

fn connection_body(spec: &CredentialConnectionSpec<'_>) -> Value {
    let mut body = json!({
        "connection_name": spec.name,
        // The phones' own credentials call through it, and our server
        // dials them as `sip:gencred...@sip.telnyx.com` from the account.
        "sip_uri_calling_preference": "internal",
        "webhook_event_url": spec.webhook_url,
        "webhook_api_version": "2",
        "webhook_timeout_secs": 10,
        "inbound": { "ani_number_format": "+E.164", "dnis_number_format": "+e164" },
        "outbound": {
            "call_parking_enabled": true,
            "outbound_voice_profile_id": spec.outbound_voice_profile_id,
        },
    });
    if let Some(id) = spec.android_push_credential_id {
        body["android_push_credential_id"] = json!(id);
    }
    if let Some(id) = spec.ios_push_credential_id {
        body["ios_push_credential_id"] = json!(id);
    }
    body
}

/// Letters and digits, `len` long, from the system's random source.
fn random_alphanumeric(len: usize) -> Result<String, TelnyxError> {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = vec![0u8; len];
    getrandom::fill(&mut bytes).map_err(|_| TelnyxError::Unexpected("no random source"))?;
    Ok(bytes
        .iter()
        .map(|byte| char::from(ALPHABET[usize::from(*byte) % ALPHABET.len()]))
        .collect())
}

impl Telnyx {
    /// A billing group, so the company's costs show apart in Telnyx
    /// (`POST /billing_groups`); its id.
    pub async fn create_billing_group(&self, name: &str) -> Result<String, TelnyxError> {
        let answer = self
            .json(
                Method::POST,
                "/billing_groups",
                Some(&json!({ "name": name })),
            )
            .await?;
        data_id(&answer, "billing group without id")
    }

    /// An outbound voice profile (`POST /outbound_voice_profiles`); its id.
    pub async fn create_voice_profile(
        &self,
        spec: &VoiceProfileSpec<'_>,
    ) -> Result<String, TelnyxError> {
        let mut body = json!({
            "name": spec.name,
            "traffic_type": "conversational",
            "service_plan": "global",
            "whitelisted_destinations": DESTINATIONS,
            "daily_spend_limit": spec.daily_spend_limit,
            "daily_spend_limit_enabled": true,
        });
        if let Some(id) = spec.billing_group_id {
            body["billing_group_id"] = json!(id);
        }
        let answer = self
            .json(Method::POST, "/outbound_voice_profiles", Some(&body))
            .await?;
        data_id(&answer, "voice profile without id")
    }

    /// A TeXML application (`POST /texml_applications`); its id.
    pub async fn create_texml_app(&self, spec: &TexmlAppSpec<'_>) -> Result<String, TelnyxError> {
        let answer = self
            .json(
                Method::POST,
                "/texml_applications",
                Some(&texml_app_body(spec)),
            )
            .await?;
        data_id(&answer, "TeXML application without id")
    }

    /// Points an existing TeXML application at our callbacks again
    /// (`PATCH /texml_applications/{id}`), keeping its name. A deleted one
    /// answers [`TelnyxError::Status`] 404.
    pub async fn update_texml_app(
        &self,
        id: &str,
        spec: &TexmlAppSpec<'_>,
    ) -> Result<(), TelnyxError> {
        let mut body = texml_app_body(spec);
        if let Some(fields) = body.as_object_mut() {
            fields.remove("friendly_name");
        }
        self.send(
            Method::PATCH,
            &format!("/texml_applications/{}", segment(id)),
            Some(&body),
        )
        .await
        .map(drop)
    }

    /// A credential connection (`POST /credential_connections`), with a
    /// random SIP login of its own that nothing uses (the phones have
    /// their own credentials on it); its id.
    pub async fn create_credential_connection(
        &self,
        spec: &CredentialConnectionSpec<'_>,
    ) -> Result<String, TelnyxError> {
        let mut body = connection_body(spec);
        body["user_name"] = json!(format!("gm{}", random_alphanumeric(20)?));
        body["password"] = json!(random_alphanumeric(32)?);
        let answer = self
            .json(Method::POST, "/credential_connections", Some(&body))
            .await?;
        data_id(&answer, "credential connection without id")
    }

    /// Points an existing credential connection at our callbacks again
    /// (`PATCH /credential_connections/{id}`), keeping its name. A deleted
    /// one answers [`TelnyxError::Status`] 404.
    pub async fn update_credential_connection(
        &self,
        id: &str,
        spec: &CredentialConnectionSpec<'_>,
    ) -> Result<(), TelnyxError> {
        let mut body = connection_body(spec);
        if let Some(fields) = body.as_object_mut() {
            fields.remove("connection_name");
        }
        self.send(
            Method::PATCH,
            &format!("/credential_connections/{}", segment(id)),
            Some(&body),
        )
        .await
        .map(drop)
    }

    /// A messaging profile (`POST /messaging_profiles`), MMS shrunk for
    /// carriers, with a daily spend limit, and the extra opt-out words; its
    /// id.
    pub async fn create_messaging_profile(
        &self,
        spec: &MessagingProfileSpec<'_>,
    ) -> Result<String, TelnyxError> {
        let body = json!({
            "name": spec.name,
            "enabled": true,
            "whitelisted_destinations": DESTINATIONS,
            "webhook_url": spec.webhook_url,
            "webhook_api_version": "2",
            "mms_transcoding": true,
            "daily_spend_limit": spec.daily_spend_limit,
            "daily_spend_limit_enabled": true,
        });
        let answer = self
            .json(Method::POST, "/messaging_profiles", Some(&body))
            .await?;
        let id = data_id(&answer, "messaging profile without id")?;
        let opt_out = json!({
            "op": "stop",
            "keywords": OPT_OUT_KEYWORDS,
            "resp_text": OPT_OUT_REPLY,
            "country_code": "US",
        });
        self.send(
            Method::POST,
            &format!("/messaging_profiles/{}/autoresp_configs", segment(&id)),
            Some(&opt_out),
        )
        .await?;
        Ok(id)
    }

    /// Points an existing messaging profile's webhook at us again
    /// (`PATCH /messaging_profiles/{id}`). A deleted one answers
    /// [`TelnyxError::Status`] 404.
    pub async fn update_messaging_webhook(
        &self,
        id: &str,
        webhook_url: &str,
    ) -> Result<(), TelnyxError> {
        self.send(
            Method::PATCH,
            &format!("/messaging_profiles/{}", segment(id)),
            Some(&json!({ "webhook_url": webhook_url, "webhook_api_version": "2" })),
        )
        .await
        .map(drop)
    }

    /// The app's push credentials in the account, by alias
    /// (`GET /mobile_push_credentials`).
    pub async fn push_credentials(&self) -> Result<PushCredentials, TelnyxError> {
        let answer = self
            .json(Method::GET, "/mobile_push_credentials?page[size]=250", None)
            .await?;
        let mut found = PushCredentials::default();
        for row in answer["data"].as_array().into_iter().flatten() {
            let id = row["id"].as_str().filter(|id| !id.is_empty());
            match (row["alias"].as_str(), id) {
                (Some(ANDROID_PUSH_ALIAS), Some(id)) => found.android = Some(id.to_string()),
                (Some(IOS_PUSH_ALIAS), Some(id)) => found.ios = Some(id.to_string()),
                _ => {}
            }
        }
        Ok(found)
    }

    /// Local US numbers with voice and texting in an area code
    /// (`GET /available_phone_numbers`), at most `limit`.
    pub async fn search_numbers(
        &self,
        area_code: &str,
        limit: u32,
    ) -> Result<Vec<AvailableNumber>, TelnyxError> {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("filter[country_code]", "US")
            .append_pair("filter[national_destination_code]", area_code)
            .append_pair("filter[phone_number_type]", "local")
            .append_pair("filter[features][]", "voice")
            .append_pair("filter[features][]", "sms")
            .append_pair("filter[limit]", &limit.to_string())
            .finish();
        let answer = self
            .json(
                Method::GET,
                &format!("/available_phone_numbers?{query}"),
                None,
            )
            .await?;
        let text = |value: &Value| {
            value
                .as_str()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        };
        Ok(answer["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|row| {
                let region = |kind: &str| {
                    row["region_information"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|region| region["region_type"] == kind)
                        .and_then(|region| text(&region["region_name"]))
                };
                Some(AvailableNumber {
                    e164: text(&row["phone_number"])?,
                    locality: region("location").or_else(|| region("rate_center")),
                    region: region("state"),
                    monthly_cost: text(&row["cost_information"]["monthly_cost"]),
                    upfront_cost: text(&row["cost_information"]["upfront_cost"]),
                    currency: text(&row["cost_information"]["currency"]),
                })
            })
            .collect())
    }

    /// Buys a number onto the company's TeXML app, messaging profile and
    /// billing group (`POST /number_orders`).
    pub async fn order_number(
        &self,
        e164: &str,
        connection_id: &str,
        messaging_profile_id: Option<&str>,
        billing_group_id: Option<&str>,
        customer_reference: &str,
    ) -> Result<NumberOrder, TelnyxError> {
        let mut body = json!({
            "phone_numbers": [{ "phone_number": e164 }],
            "connection_id": connection_id,
            "customer_reference": customer_reference,
        });
        if let Some(id) = messaging_profile_id {
            body["messaging_profile_id"] = json!(id);
        }
        if let Some(id) = billing_group_id {
            body["billing_group_id"] = json!(id);
        }
        let answer = self
            .json(Method::POST, "/number_orders", Some(&body))
            .await?;
        Ok(NumberOrder {
            id: data_id(&answer, "number order without id")?,
            status: answer["data"]["status"]
                .as_str()
                .unwrap_or("pending")
                .to_string(),
        })
    }

    /// Puts one of the account's numbers on a connection and billing group
    /// (`PATCH /phone_numbers/{id}`).
    pub async fn assign_number(
        &self,
        id: &str,
        connection_id: &str,
        billing_group_id: Option<&str>,
    ) -> Result<(), TelnyxError> {
        let mut body = json!({ "connection_id": connection_id });
        if let Some(billing_group_id) = billing_group_id {
            body["billing_group_id"] = json!(billing_group_id);
        }
        self.send(
            Method::PATCH,
            &format!("/phone_numbers/{}", segment(id)),
            Some(&body),
        )
        .await
        .map(drop)
    }

    /// Gives one of the account's numbers a messaging profile
    /// (`PATCH /phone_numbers/{id}/messaging`).
    pub async fn set_number_messaging(
        &self,
        id: &str,
        messaging_profile_id: &str,
    ) -> Result<(), TelnyxError> {
        self.send(
            Method::PATCH,
            &format!("/phone_numbers/{}/messaging", segment(id)),
            Some(&json!({ "messaging_profile_id": messaging_profile_id })),
        )
        .await
        .map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_may_be_text_or_numbers() {
        assert_eq!(
            data_id(&json!({ "data": { "id": "abc" } }), "x").unwrap(),
            "abc"
        );
        assert_eq!(
            data_id(
                &json!({ "data": { "id": 3_077_399_364_935_615_624_u64 } }),
                "x"
            )
            .unwrap(),
            "3077399364935615624"
        );
        assert!(data_id(&json!({ "data": {} }), "x").is_err());
        assert!(data_id(&json!({ "data": { "id": "" } }), "x").is_err());
    }

    #[test]
    fn connections_take_the_push_credentials_given() {
        let spec = CredentialConnectionSpec {
            name: "gm-1",
            webhook_url: "https://api.test/hook/outbound",
            outbound_voice_profile_id: "ovp-1",
            android_push_credential_id: Some("push-a"),
            ios_push_credential_id: None,
        };
        let body = connection_body(&spec);
        assert_eq!(body["android_push_credential_id"], "push-a");
        assert!(body.get("ios_push_credential_id").is_none());
        assert_eq!(body["outbound"]["call_parking_enabled"], true);
        assert_eq!(body["sip_uri_calling_preference"], "internal");
    }

    #[test]
    fn random_logins_are_letters_and_digits() {
        let login = random_alphanumeric(20).unwrap();
        assert_eq!(login.len(), 20);
        assert!(login.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(login, random_alphanumeric(20).unwrap());
    }
}
