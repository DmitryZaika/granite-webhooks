//! Telnyx's callbacks for Granite Phone, under
//! `/v1/webhooks/telnyx/{token}/...`, where `token` is the company's
//! `telnyx_company_config.webhook_token` (the end of `webhookBase`).
//!
//! - `POST .../inbound`                     → TeXML: ring the number's phones, or its no-answer action
//! - `POST .../dial-result?call=`           → TeXML: hang up, or the next no-answer step (cells, AI, voicemail, forward)
//! - `POST .../cell-screen?call=&user=`     → TeXML: "press 1" (or the user's PIN) to answer on a cell (fallback)
//! - `POST .../whisper?call=&user=`         → TeXML: the announcement a user hears before a call connects
//! - `POST .../cell-screen-result?call=&user=` → TeXML: connect the cell, or hang it up
//! - `POST .../after-ai?call=`              → TeXML: voicemail when the AI Assistant did not start, else hang up
//! - `POST .../voicemail-done?call=`        → TeXML: hang up (a voicemail was left)
//! - `POST .../leg-status?call=&device=`    → a phone's or the customer's leg rang, answered or ended
//! - `POST .../call-status`                 → a call ended: final status and talk time
//! - `POST .../recording?call=&kind=`       → copy the recording to our private bucket
//! - `POST .../outbound`                    → Call Control: approve the app's parked calls, or hang up
//! - `POST .../via-cell?call=`, `.../via-cell-gather?call=`, `.../notice` → Call via my cell
//! - `POST .../test-call?call=`, `.../cell-code?user=` → test calls, cell verification codes
//! - `POST .../porting`                     → port-in notifications (Ed25519, required; `porting.rs`)
//! - `POST .../messaging`                   → texts: inbound texts and MMS, delivery status (Ed25519-signed, required)
//! - `POST .../10dlc`                       → texting registration: brand, campaign and number updates (Ed25519, required; `tendlc.rs`)
//!
//! No session. Call Control (`outbound`) proves itself with Telnyx's
//! Ed25519 signature (`crate::telnyx::verify_signature`), always required:
//! without `TELNYX_PUBLIC_KEY` those events are refused (503, logged)
//! unless `PHONE_ALLOW_UNSIGNED_WEBHOOKS=1`. Telnyx documents no signature
//! for TeXML requests, so they are authenticated by the path's webhook
//! token and, when the company saved its Telnyx account id, the `AccountSid`
//! every TeXML request carries (required then); signature headers that do
//! come are checked (a bad one is 401). The connection id a request names
//! and the number `inbound` is called on must belong to the token's company
//! (403 otherwise, logged without numbers). Durations Telnyx sends are held
//! to 0 to 24 hours. TeXML requests are form-encoded POSTs (the TeXML app's
//! voice and status-callback methods must be POST); answers are TeXML
//! (`application/xml`). Every callback is stored (`phone_call_events`) and
//! claimed before it changes anything, so a repeat changes nothing.

mod dial;
mod inbound;
mod messaging;
mod misc;
mod outbound;
mod porting;
mod recording;
mod status;
mod tendlc;
mod via_cell;
mod whisper;

use crate::error::{ApiError, ApiResult};
use crate::phone::PhoneConfig;
use crate::phone::call_log::MAX_TALK_SECS;
use crate::phone::company::{self, CompanyConfig, RecordingMode};
use crate::phone::events::{self, Event, Source};
use crate::phone::numbers::spoken_digits;
use crate::phone::routing::{NoAnswer, RingPhone};
use crate::state::AppState;
use crate::telnyx::{Response as Texml, Verb, verify_signature};
use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use lambda_http::tracing;
use serde::Deserialize;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(inbound::inbound))
        .routes(routes!(dial::dial_result))
        .routes(routes!(dial::cell_screen))
        .routes(routes!(dial::cell_screen_result))
        .routes(routes!(dial::after_ai))
        .routes(routes!(dial::voicemail_done))
        .routes(routes!(whisper::whisper))
        .routes(routes!(status::leg_status))
        .routes(routes!(status::call_status))
        .routes(routes!(recording::recording))
        .routes(routes!(outbound::outbound))
        .routes(routes!(via_cell::via_cell))
        .routes(routes!(via_cell::via_cell_gather))
        .routes(routes!(via_cell::notice))
        .routes(routes!(misc::test_call))
        .routes(routes!(misc::cell_code))
        .routes(routes!(porting::porting))
        .routes(routes!(messaging::messaging))
        .routes(routes!(tendlc::take_telnyx_10dlc_event))
}

/// What callers hear when their call may be recorded.
const NOTICE: &str = "This call may be recorded.";
/// What a caller hears when we fail and the company has no fallback TeXML.
const SORRY: &str = "Sorry, we cannot take your call right now. Please call again later.";
/// Telnyx's signature headers.
const SIGNATURE_HEADER: &str = "telnyx-signature-ed25519";
const TIMESTAMP_HEADER: &str = "telnyx-timestamp";

/// The fields of Telnyx's TeXML requests the callbacks read. Telnyx posts
/// them form-encoded; every field is optional and others are ignored.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
pub struct TexmlRequest {
    /// The Telnyx account; required, and must be the company's, when the
    /// company saved its account id (`telnyxAccountId`).
    pub account_sid: Option<String>,
    /// The call, or the leg, the request is about.
    pub call_sid: Option<String>,
    /// The Telnyx connection (the company's TeXML app); must be the
    /// company's when it set one.
    pub connection_id: Option<String>,
    /// Who placed the call: E.164, or a SIP URI.
    pub from: Option<String>,
    /// Who was called: E.164, or a SIP URI.
    pub to: Option<String>,
    /// The call's status: `initiated`, `ringing`, `in-progress`,
    /// `completed`, `busy`, `no-answer`, `canceled` or `failed`; after an AI
    /// Assistant that could not start, `conversation_start_failed`.
    pub call_status: Option<String>,
    /// How long the call (or the leg) lasted, in seconds (0 to 86400).
    pub call_duration: Option<String>,
    /// How a `<Dial>` ended: `completed` (answered), `busy`, `no-answer`,
    /// `canceled` or `failed`.
    pub dial_call_status: Option<String>,
    /// The leg a `<Dial>` connected.
    pub dial_call_sid: Option<String>,
    /// How long that leg talked, in seconds (0 to 86400).
    pub dial_call_duration: Option<String>,
    /// The recording (its Telnyx id).
    pub recording_sid: Option<String>,
    /// The recording's status: `in-progress`, `completed` or `absent`.
    pub recording_status: Option<String>,
    /// The recording's length, in seconds.
    pub recording_duration: Option<String>,
    /// The keys pressed at a `<Gather>`.
    pub digits: Option<String>,
    /// Why the call ended (Telnyx's hangup cause).
    pub hangup_cause: Option<String>,
    /// Why the AI Assistant's conversation ended or did not start.
    pub reason: Option<String>,
    /// When it happened (RFC 3339 or RFC 2822); used as the time of an
    /// answer or an end, held between the call's start and now.
    pub timestamp: Option<String>,
    /// The caller's display name, when the network gave one (Telnyx's
    /// inbound call screening puts `SPAM LIKELY` there).
    pub caller_id: Option<String>,
    /// The caller-ID check's (STIR/SHAKEN) attestation, `A`, `B` or `C`,
    /// when the TeXML app sends STIR/SHAKEN data.
    pub shaken_stir_attestation: Option<String>,
    /// Whether the caller-ID check passed (`true` / `false`), when sent.
    pub shaken_stir_validated: Option<String>,
}

/// A field as sent, trimmed; `None` when missing or blank.
fn field(value: Option<&String>) -> Option<&str> {
    value
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
}

impl TexmlRequest {
    fn account_sid(&self) -> Option<&str> {
        field(self.account_sid.as_ref())
    }

    fn call_sid(&self) -> Option<&str> {
        field(self.call_sid.as_ref())
    }

    fn connection_id(&self) -> Option<&str> {
        field(self.connection_id.as_ref())
    }

    fn from(&self) -> Option<&str> {
        field(self.from.as_ref())
    }

    fn to(&self) -> Option<&str> {
        field(self.to.as_ref())
    }

    fn call_status(&self) -> Option<&str> {
        field(self.call_status.as_ref())
    }

    fn call_duration(&self) -> Option<i64> {
        seconds(field(self.call_duration.as_ref()))
    }

    fn dial_call_status(&self) -> Option<&str> {
        field(self.dial_call_status.as_ref())
    }

    fn dial_call_sid(&self) -> Option<&str> {
        field(self.dial_call_sid.as_ref())
    }

    fn dial_call_duration(&self) -> Option<i64> {
        seconds(field(self.dial_call_duration.as_ref()))
    }

    fn recording_sid(&self) -> Option<&str> {
        field(self.recording_sid.as_ref())
    }

    fn recording_status(&self) -> Option<&str> {
        field(self.recording_status.as_ref())
    }

    fn recording_duration(&self) -> Option<i64> {
        seconds(field(self.recording_duration.as_ref()))
    }

    fn digits(&self) -> Option<&str> {
        field(self.digits.as_ref())
    }

    fn hangup_cause(&self) -> Option<&str> {
        field(self.hangup_cause.as_ref())
    }

    fn timestamp(&self) -> Option<&str> {
        field(self.timestamp.as_ref())
    }

    /// The call is likely spam: its STIR/SHAKEN validation failed, or
    /// Telnyx's call screening named it `SPAM LIKELY`. A call without that
    /// data is not (attestation C alone is not either, as Telnyx's own
    /// screening no longer counts it).
    fn spam_likely(&self) -> bool {
        let failed = field(self.shaken_stir_validated.as_ref())
            .is_some_and(|validated| validated.eq_ignore_ascii_case("false"));
        let flagged = field(self.caller_id.as_ref())
            .is_some_and(|name| name.to_ascii_uppercase().contains("SPAM LIKELY"));
        failed || flagged
    }

    /// When the callback says it happened.
    fn occurred_at(&self) -> Option<chrono::DateTime<Utc>> {
        events::parse_time(self.timestamp())
    }
}

/// Whole seconds Telnyx wrote as `61` or `61.4`, held to 0 ...
/// [`MAX_TALK_SECS`] (a day): nothing longer is believed.
fn seconds(text: Option<&str>) -> Option<i64> {
    let value: f64 = text?.parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    let secs = value.min(1e12).round() as i64;
    Some(secs.min(MAX_TALK_SECS))
}

/// The company whose callbacks carry `token`: 404 for none.
async fn company_for(state: &AppState, token: &str) -> ApiResult<CompanyConfig> {
    let token = token.trim();
    if token.is_empty() || token.len() > 64 {
        return Err(ApiError::NotFound);
    }
    company::by_webhook_token(&state.pool, token)
        .await?
        .ok_or(ApiError::NotFound)
}

/// Telnyx's signature: checked when the headers come, and with `required`
/// (Call Control) they must; 401 when it does not check out. Without
/// `TELNYX_PUBLIC_KEY` nothing can be checked: a `required` one is refused
/// (503, logged as an error) unless `PHONE_ALLOW_UNSIGNED_WEBHOOKS=1`.
fn verify(state: &AppState, headers: &HeaderMap, body: &[u8], required: bool) -> ApiResult<()> {
    let Some(key) = state.phone.public_key.as_deref() else {
        if required && !state.phone.allow_unsigned_webhooks {
            tracing::error!(
                "TELNYX_PUBLIC_KEY is not set: a Call Control event was refused \
                 (set it, or PHONE_ALLOW_UNSIGNED_WEBHOOKS=1 for local testing)"
            );
            return Err(ApiError::Unavailable(
                "Telnyx signatures are not set up on the server.".into(),
            ));
        }
        return Ok(());
    };
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    match (header(SIGNATURE_HEADER), header(TIMESTAMP_HEADER)) {
        (Some(signature), Some(timestamp))
            if verify_signature(key, timestamp, body, signature, Utc::now().timestamp()) =>
        {
            Ok(())
        }
        (None, None) if !required => Ok(()),
        _ => Err(ApiError::Unauthorized),
    }
}

/// 403 for a request that names another company's account, connection or
/// number (logged with ids only).
fn refused(config: &CompanyConfig, route: &'static str, what: &'static str) -> ApiError {
    tracing::warn!(
        company_id = config.company_id,
        route,
        what,
        "Telnyx callback refused: not the token's company"
    );
    ApiError::Forbidden
}

/// The account a TeXML request names (Telnyx sends `AccountSid` with every
/// one) is the company's, when it saved its account id: a request without
/// it is refused then too.
fn check_account(
    config: &CompanyConfig,
    route: &'static str,
    account: Option<&str>,
) -> ApiResult<()> {
    match config.telnyx_account_id.as_deref() {
        Some(ours) if account != Some(ours) => Err(refused(config, route, "account")),
        _ => Ok(()),
    }
}

/// The connection a request names is one of `ours` (the company's), when it
/// set any.
fn check_connection(
    config: &CompanyConfig,
    route: &'static str,
    connection: Option<&str>,
    ours: &[Option<&str>],
) -> ApiResult<()> {
    let ours: Vec<&str> = ours.iter().flatten().copied().collect();
    match connection {
        Some(theirs) if !ours.is_empty() && !ours.contains(&theirs) => {
            Err(refused(config, route, "connection"))
        }
        _ => Ok(()),
    }
}

/// A TeXML callback that passed its checks.
struct Hook {
    config: CompanyConfig,
    form: TexmlRequest,
    /// The body as Telnyx sent it (stored with the event).
    body: String,
    /// The root of the company's callbacks.
    base: String,
}

/// A stored callback: whether it was new (its side effects ours to run),
/// and its key (to give it back when handling it failed).
struct Claim {
    new: bool,
    key: String,
}

/// The company's TeXML callback `route`: 404 for an unknown token, 401 for
/// a bad signature, 400 for a body that is not a form, 403 for another
/// company's account or connection.
async fn texml(
    state: &AppState,
    token: &str,
    headers: &HeaderMap,
    body: &Bytes,
    route: &'static str,
) -> ApiResult<Hook> {
    let config = company_for(state, token).await?;
    verify(state, headers, body, false)?;
    let form: TexmlRequest = serde_html_form::from_bytes(body)
        .map_err(|_| ApiError::BadRequest("Not a form-encoded TeXML request".into()))?;
    check_account(&config, route, form.account_sid())?;
    check_connection(
        &config,
        route,
        form.connection_id(),
        &[
            config.texml_app_id.as_deref(),
            config.credential_connection_id.as_deref(),
        ],
    )?;
    let base = config.webhook_base(&state.phone);
    Ok(Hook {
        config,
        form,
        body: String::from_utf8_lossy(body).into_owned(),
        base,
    })
}

impl Hook {
    const fn company_id(&self) -> i32 {
        self.config.company_id
    }

    /// The dedupe key of this callback as `kind` (the route and step).
    fn key(&self, kind: &str) -> String {
        let form = &self.form;
        events::texml_key(
            form.call_sid(),
            kind,
            form.call_status(),
            form.recording_sid(),
            form.dial_call_status(),
        )
    }

    fn event<'a>(&'a self, kind: &'a str, key: &'a str, call_id: Option<i64>) -> Event<'a> {
        Event {
            company_id: self.config.company_id,
            call_id,
            source: Source::Texml,
            event_type: kind,
            dedupe_key: key,
            occurred_at: events::parse_time(self.form.timestamp()),
            payload: &self.body,
        }
    }

    /// Stores and claims this callback as `kind`.
    async fn claim(
        &self,
        state: &AppState,
        kind: &str,
        call_id: Option<i64>,
    ) -> sqlx::Result<Claim> {
        let key = self.key(kind);
        let new = events::claim(&state.pool, &self.event(kind, &key, call_id)).await?;
        Ok(Claim { new, key })
    }
}

/// `200` with no body: what status callbacks answer.
fn taken() -> Response {
    StatusCode::OK.into_response()
}

/// `<Response><Hangup/></Response>`.
fn hang_up() -> Response {
    Texml::new().hangup().into_response()
}

/// What a caller gets when we fail: the company's fallback TeXML (else
/// `TELNYX_FALLBACK_TEXML_URL`), or an apology.
fn fallback(config: &CompanyConfig, phone: &PhoneConfig) -> Response {
    config
        .fallback_texml_url
        .as_deref()
        .or(phone.fallback_texml_url.as_deref())
        .map_or_else(
            || Texml::new().say(SORRY).hangup(),
            |url| Texml::new().redirect(url),
        )
        .into_response()
}

/// Logs a callback that failed (no numbers: the error and ids only).
fn failed(route: &'static str, company_id: i32, err: &ApiError) {
    match err {
        ApiError::Database(err) => {
            tracing::error!(route, company_id, error = %err, "Telnyx callback failed");
        }
        other => tracing::error!(route, company_id, error = %other, "Telnyx callback failed"),
    }
}

/// Incoming calls are recorded (`inbound` or `both`).
const fn records_inbound(config: &CompanyConfig) -> bool {
    matches!(
        config.recording_mode,
        RecordingMode::Inbound | RecordingMode::Both
    )
}

/// Outgoing calls are recorded too (`both`).
const fn records_outbound(config: &CompanyConfig) -> bool {
    matches!(config.recording_mode, RecordingMode::Both)
}

/// A `<Dial>` that records both sides from the answer, when `record`.
fn recorded(dial: Verb, record: bool, base: &str, call_id: i64) -> Verb {
    if !record {
        return dial;
    }
    dial.attr("record", "record-from-answer-dual").attr(
        "recordingStatusCallback",
        format!("{base}/recording?call={call_id}&kind=call"),
    )
}

/// How the phones of an incoming call ring, besides the phones.
#[derive(Debug, Clone, Default)]
struct RingOptions {
    /// What the phones show as the caller (`<Dial callerId>`); None = the
    /// caller's own number.
    caller_id: Option<String>,
    /// The users who hear an announcement before the call connects.
    announced: Vec<i32>,
}

/// Rings the phones, all at once, for `timeout` seconds.
fn ring(
    response: Texml,
    phones: &[RingPhone],
    timeout: i64,
    base: &str,
    call_id: i64,
    record: bool,
    options: &RingOptions,
) -> Texml {
    let dial = Verb::new("Dial")
        .attr("answerOnBridge", "true")
        .attr("timeout", timeout.to_string())
        .attr("action", format!("{base}/dial-result?call={call_id}"))
        .attr_opt("callerId", options.caller_id.clone());
    let mut dial = recorded(dial, record, base, call_id);
    for phone in phones {
        let whisper = options
            .announced
            .contains(&phone.user_id)
            .then(|| format!("{base}/whisper?call={call_id}&user={}", phone.user_id));
        dial = dial.child(
            Verb::new("Sip")
                .attr_opt("url", whisper)
                .attr(
                    "statusCallback",
                    format!(
                        "{base}/leg-status?call={call_id}&device={}",
                        phone.device_id
                    ),
                )
                .attr(
                    "statusCallbackEvent",
                    "initiated ringing answered completed",
                )
                .text(format!("sip:{}@sip.telnyx.com", phone.sip_username)),
        );
    }
    response.verb(dial)
}

/// A greeting, then a voicemail of up to 2 minutes.
fn voicemail(response: Texml, greeting: &str, base: &str, call_id: i64) -> Texml {
    response.say(greeting).verb(
        Verb::new("Record")
            .attr("maxLength", "120")
            .attr("playBeep", "true")
            .attr("action", format!("{base}/voicemail-done?call={call_id}"))
            .attr(
                "recordingStatusCallback",
                format!("{base}/recording?call={call_id}&kind=voicemail"),
            ),
    )
}

/// The TeXML of a no-answer step (cells ring for `timeout` seconds).
fn no_answer(
    response: Texml,
    step: &NoAnswer,
    base: &str,
    call_id: i64,
    timeout: i64,
    record: bool,
) -> Texml {
    match step {
        NoAnswer::Cells(cells) => {
            let dial = Verb::new("Dial").attr("timeout", timeout.to_string()).attr(
                "action",
                format!("{base}/dial-result?call={call_id}&cell=1"),
            );
            let mut dial = recorded(dial, record, base, call_id);
            for (user_id, cell) in cells {
                dial = dial.child(
                    Verb::new("Number")
                        .attr(
                            "url",
                            format!("{base}/cell-screen?call={call_id}&user={user_id}"),
                        )
                        .text(cell.clone()),
                );
            }
            response.verb(dial)
        }
        NoAnswer::Ai(assistant_id) => response.verb(
            Verb::new("Connect")
                .attr("action", format!("{base}/after-ai?call={call_id}"))
                .child(Verb::new("AIAssistant").attr("id", assistant_id.clone())),
        ),
        NoAnswer::Voicemail(greeting) => voicemail(response, greeting, base, call_id),
        NoAnswer::Forward(number) => response.verb(Verb::new("Dial").text(number.clone())),
        NoAnswer::Message(text) => response.say(text).hangup(),
    }
}

/// Who is on the other end, for text to speech: the customer's name, else
/// the number read out digit by digit.
async fn other_party(
    state: &AppState,
    call: &crate::phone::call_log::CallFacts,
) -> sqlx::Result<String> {
    let name =
        crate::phone::call_log::customer_name(&state.pool, call.company_id, call.customer_id)
            .await?;
    Ok(name.unwrap_or_else(|| {
        let digits = spoken_digits(&call.other_number);
        if digits.is_empty() {
            "an unknown number".to_string()
        } else {
            digits
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_are_whole_seconds() {
        assert_eq!(seconds(Some("61")), Some(61));
        assert_eq!(seconds(Some("61.6")), Some(62));
        assert_eq!(seconds(Some("0")), Some(0));
        assert_eq!(seconds(Some("-3")), None);
        assert_eq!(seconds(Some("NaN")), None);
        assert_eq!(seconds(Some("inf")), None);
        assert_eq!(seconds(Some("soon")), None);
        assert_eq!(seconds(None), None);
        // A day at most, however large the number.
        assert_eq!(seconds(Some("86401")), Some(86_400));
        assert_eq!(seconds(Some("9223372036854775807")), Some(86_400));
        assert_eq!(seconds(Some("1e300")), Some(86_400));
    }

    #[test]
    fn a_saved_account_id_must_be_named() {
        let mut config = crate::phone::company::CompanyConfig {
            company_id: 100,
            enabled: true,
            company_deleted: false,
            webhook_token: "t".into(),
            credential_connection_id: None,
            texml_app_id: None,
            outbound_voice_profile_id: None,
            billing_group_id: None,
            telnyx_account_id: None,
            ai_assistant_id: None,
            fallback_texml_url: None,
            recording_mode: RecordingMode::Inbound,
            recording_notice_url: None,
            messaging_profile_id: None,
            business_hours: None,
            holidays: Vec::new(),
            min_app_version: None,
            daily_call_limit: 300,
        };
        assert!(check_account(&config, "inbound", None).is_ok());
        assert!(check_account(&config, "inbound", Some("acct-9")).is_ok());
        config.telnyx_account_id = Some("acct-100".into());
        assert!(check_account(&config, "inbound", Some("acct-100")).is_ok());
        assert!(check_account(&config, "inbound", Some("acct-9")).is_err());
        assert!(check_account(&config, "inbound", None).is_err());
    }

    #[test]
    fn telnyx_forms_are_read_with_unknown_fields_ignored() {
        let form: TexmlRequest = serde_html_form::from_bytes(
            b"AccountSid=acct-1&CallSid=v3%3Aabc&From=%2B13175550142&To=%2B13175550151&Digits=+1+&Extra=x&RecordingDuration=",
        )
        .unwrap();
        assert_eq!(form.account_sid(), Some("acct-1"));
        assert_eq!(form.call_sid(), Some("v3:abc"));
        assert_eq!(form.from(), Some("+13175550142"));
        assert_eq!(form.to(), Some("+13175550151"));
        assert_eq!(form.digits(), Some("1"));
        assert_eq!(form.recording_duration(), None);
        assert_eq!(form.dial_call_status(), None);
    }
}
