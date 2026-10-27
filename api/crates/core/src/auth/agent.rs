//! Agent tokens: how the claude-job-runner's agent reads the API as the
//! person who asked it a question, and nothing more.
//!
//! The runner trades `AGENT_RUNNER_KEY` for a token bound to one queued
//! question (`POST /v1/ai/agentic-questions/{id}/agent-token`). The agent
//! sends it as `Authorization: Bearer agt_...` and every route then sees the
//! asker, in the company the question was asked in, with these limits:
//!
//! - it only reads (`GET`/`HEAD`), plus [`WRITABLE_ROUTES`];
//! - it is never a superuser, and a route naming a company
//!   (`{company_id}`) must name that one, so it cannot leave the company;
//! - it works only while the question is open (not yet `done` or `failed`)
//!   and for [`TOKEN_TTL_SECS`] after it was issued.
//!
//! A token is `agt_` + base64url(claims) + `.` + base64url(signature), the
//! signature an HMAC-SHA256 under `SESSION_SECRET` of [`SIGNED_PREFIX`] and the
//! claims. Session cookies sign bare base64, which never holds a `.`, so the
//! two can never stand in for each other.

use super::constant_time_eq;
use super::session::{
    SessionUser, UserPositionRow, effective_user, find_live_user, user_positions,
};
use crate::error::ApiError;
use crate::state::AppState;
use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use sqlx::MySqlPool;

type HmacSha256 = Hmac<Sha256>;

/// What every agent token starts with.
pub const TOKEN_PREFIX: &str = "agt_";
/// Signed ahead of the claims, keeping agent signatures apart from cookies'.
const SIGNED_PREFIX: &str = "agent-token-v1.";
/// How long a token works after it is issued.
pub const TOKEN_TTL_SECS: i64 = 30 * 60;
/// Questions older than this get no token: nobody is waiting any more.
pub const MAX_QUESTION_AGE_DAYS: i32 = 7;
/// The only routes, besides reads, an agent may call.
pub const WRITABLE_ROUTES: [&str; 1] = ["/v1/call-transcripts"];

/// Whom a token acts for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    /// The question (`chat_messages.id`).
    pub q: i64,
    /// The asker (`chats.user_id`).
    pub u: i32,
    /// The company the question was asked in (`chats.company_id`).
    pub c: i32,
    /// Unix seconds after which the token is refused.
    pub exp: i64,
}

fn signature(secret: &str, claims: &str) -> Option<HmacSha256> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(SIGNED_PREFIX.as_bytes());
    mac.update(claims.as_bytes());
    Some(mac)
}

/// A token for `claims`, signed under `secret`.
pub fn issue(secret: &str, claims: &Claims) -> String {
    let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).expect("claims serialize"));
    let mac = signature(secret, &body).expect("HMAC accepts any key length");
    let signed = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    format!("{TOKEN_PREFIX}{body}.{signed}")
}

/// The claims of a token signed under `secret` that has not expired at `now`
/// (unix seconds); `None` for anything else.
pub fn verify(secret: &str, token: &str, now: i64) -> Option<Claims> {
    let (body, signed) = token.strip_prefix(TOKEN_PREFIX)?.split_once('.')?;
    let signed = URL_SAFE_NO_PAD.decode(signed).ok()?;
    signature(secret, body)?.verify_slice(&signed).ok()?;
    let claims: Claims = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(body).ok()?).ok()?;
    (claims.exp > now).then_some(claims)
}

/// The agent token in an `Authorization: Bearer` header, if one is there.
pub fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let token = value.strip_prefix("Bearer ")?.trim();
    token.starts_with(TOKEN_PREFIX).then_some(token)
}

/// Agents read; of the writes, only [`WRITABLE_ROUTES`].
pub fn allowed(method: &Method, route: &str) -> bool {
    matches!(*method, Method::GET | Method::HEAD)
        || (*method == Method::POST && WRITABLE_ROUTES.contains(&route))
}

/// A route naming a company (`/v1/marketing/{company_id}/...`) must name the
/// token's. Segments are matched from the end, past any mount prefix.
pub fn same_company(route: &str, path: &str, company_id: i32) -> bool {
    let company = company_id.to_string();
    route
        .rsplit('/')
        .zip(path.rsplit('/'))
        .all(|(template, actual)| template != "{company_id}" || actual == company)
}

/// The asker of an open, recent agentic question: `(user_id, company_id)`.
pub async fn open_question(
    pool: &MySqlPool,
    question_id: i64,
) -> Result<Option<(i32, i32)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT CAST(c.user_id AS SIGNED), CAST(c.company_id AS SIGNED)
           FROM chat_messages m
           JOIN chats c ON c.id = m.chat_id
          WHERE m.id = ? AND m.sender = 'user' AND m.is_agentic = 1
            AND (m.status IS NULL OR m.status = 'pending')
            AND m.created_at > DATE_SUB(CURRENT_TIMESTAMP, INTERVAL ? DAY)",
    )
    .bind(question_id)
    .bind(MAX_QUESTION_AGE_DAYS)
    .fetch_optional(pool)
    .await
}

/// The asker as the routes see them: acting in the question's company and
/// never as a superuser. `None` when they can no longer act there.
pub async fn asker(
    pool: &MySqlPool,
    user_id: i32,
    company_id: i32,
) -> Result<Option<(SessionUser, Vec<UserPositionRow>)>, sqlx::Error> {
    let Some(user) = find_live_user(pool, user_id).await? else {
        return Ok(None);
    };
    let positions = user_positions(pool, user.id).await?;
    let user = effective_user(user, &positions, Some(company_id));
    if user.company_id != company_id || !(user.is_employee || user.is_admin) {
        return Ok(None);
    }
    Ok(Some((
        SessionUser {
            is_superuser: false,
            ..user
        },
        positions,
    )))
}

/// The user a request carrying an agent token acts as. Refuses a token that
/// is forged, expired, for a closed question or for a write it may not make.
pub async fn authenticate(
    parts: &Parts,
    state: &AppState,
    token: &str,
) -> Result<(SessionUser, Vec<UserPositionRow>), ApiError> {
    let now = chrono::Utc::now().timestamp();
    let claims = verify(&state.session_secret, token, now).ok_or(ApiError::Unauthorized)?;
    let route = parts
        .extensions
        .get::<axum::extract::MatchedPath>()
        .map_or_else(|| parts.uri.path(), axum::extract::MatchedPath::as_str);
    lambda_http::tracing::info!(
        question = claims.q,
        user = claims.u,
        company = claims.c,
        method = %parts.method,
        route,
        "agent request"
    );
    if open_question(&state.pool, claims.q).await? != Some((claims.u, claims.c)) {
        return Err(ApiError::Unauthorized);
    }
    if !allowed(&parts.method, route) {
        return Err(ApiError::Denied(
            "The agent may only read, and transcribe calls.".into(),
        ));
    }
    if !same_company(route, parts.uri.path(), claims.c) {
        return Err(ApiError::NotFound);
    }
    asker(&state.pool, claims.u, claims.c)
        .await?
        .ok_or(ApiError::Unauthorized)
}

/// The claude-job-runner, by `AGENT_RUNNER_KEY` (`Authorization: Bearer`).
/// Refused when the key is unset.
pub struct AgentRunner;

impl FromRequestParts<AppState> for AgentRunner {
    type Rejection = ApiError;

    #[allow(clippy::unused_async_trait_impl)] // the trait's signature
    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let sent = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        match (&state.agent_runner_key, sent) {
            (Some(key), Some(sent)) if constant_time_eq(key.as_bytes(), sent.as_bytes()) => {
                Ok(Self)
            }
            _ => Err(ApiError::Unauthorized),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAIMS: Claims = Claims {
        q: 9,
        u: 100,
        c: 100,
        exp: 2_000,
    };

    #[test]
    fn tokens_round_trip_until_they_expire() {
        let token = issue("secret", &CLAIMS);
        assert!(token.starts_with("agt_"));
        assert_eq!(verify("secret", &token, 1_999), Some(CLAIMS));
        assert_eq!(verify("secret", &token, 2_000), None);
    }

    #[test]
    fn forged_and_altered_tokens_are_refused() {
        let token = issue("secret", &CLAIMS);
        assert_eq!(verify("other secret", &token, 0), None);
        let (body, signed) = token.split_once('.').unwrap();
        let other = issue("secret", &Claims { c: 101, ..CLAIMS });
        let other_body = other.split_once('.').unwrap().0;
        assert_eq!(verify("secret", &format!("{other_body}.{signed}"), 0), None);
        assert_eq!(verify("secret", &format!("{body}.x{signed}"), 0), None);
        assert_eq!(verify("secret", body, 0), None);
        assert_eq!(verify("secret", &token.replacen("agt_", "", 1), 0), None);
    }

    #[test]
    fn a_session_cookie_signature_is_no_token() {
        // A cookie signs its bare base64 payload; reusing that as a token fails.
        let cookie = crate::auth::cookie::encode_session(
            &crate::auth::cookie::SessionData {
                session_id: Some("aaaaaaaa-0000-4000-8000-000000000100".into()),
                active_company_id: None,
            },
            "secret",
        );
        let cookie = percent_encoding::percent_decode_str(&cookie)
            .decode_utf8()
            .unwrap()
            .into_owned();
        assert_eq!(verify("secret", &format!("agt_{cookie}"), 0), None);
    }

    #[test]
    fn agents_only_read_and_transcribe() {
        assert!(allowed(&Method::GET, "/v1/customers/{id}"));
        assert!(allowed(&Method::HEAD, "/v1/customers/{id}"));
        assert!(allowed(&Method::POST, "/v1/call-transcripts"));
        assert!(!allowed(&Method::POST, "/v1/sms/{provider}/send"));
        assert!(!allowed(&Method::PUT, "/v1/call-transcripts"));
        assert!(!allowed(&Method::DELETE, "/v1/customers/{id}"));
        assert!(!allowed(&Method::PATCH, "/v1/deals/{id}"));
    }

    #[test]
    fn routes_naming_a_company_must_name_the_tokens() {
        let route = "/v1/marketing/{company_id}/leads/{id}";
        assert!(same_company(route, "/v1/marketing/100/leads/7", 100));
        assert!(!same_company(route, "/v1/marketing/101/leads/7", 100));
        assert!(!same_company(route, "/v1/marketing/1001/leads/7", 100));
        assert!(same_company(
            route,
            "/lambda-url/api/v1/marketing/100/leads/7",
            100
        ));
        assert!(same_company("/v1/customers/{id}", "/v1/customers/101", 100));
    }

    #[test]
    fn only_agent_bearer_tokens_are_picked_up() {
        let mut headers = HeaderMap::new();
        assert_eq!(bearer_token(&headers), None);
        headers.insert(AUTHORIZATION, "Bearer agt_abc.def".parse().unwrap());
        assert_eq!(bearer_token(&headers), Some("agt_abc.def"));
        headers.insert(AUTHORIZATION, "Bearer runner-key".parse().unwrap());
        assert_eq!(bearer_token(&headers), None);
        headers.insert(AUTHORIZATION, "agt_abc.def".parse().unwrap());
        assert_eq!(bearer_token(&headers), None);
    }
}
