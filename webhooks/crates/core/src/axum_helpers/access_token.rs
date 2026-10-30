//! Access-token auth for the webhooks routes (`common::access_token`).
//!
//! - [`AccessToken`]: the strict check. The request must carry a token that
//!   works for the route's scope and company, else 401 (no or bad token) or
//!   403 (a good token that may not call this route).
//! - [`Transitional`]: while callers move to tokens. A working token is
//!   accepted. Without one (or with one that does not work) the route's old
//!   check ([`LegacyAuth`]) decides exactly as before, and `PostHog` hears
//!   about it (`webhook_legacy_auth`, `webhook_access_token_rejected`), so we
//!   can see who still has to switch. Once nobody does, the route moves to
//!   [`AccessToken`] and its old check is deleted.
//!
//! The token is read from `Authorization` (`Bearer gmk_...` or the bare
//! token), else from the headers the providers that cannot set
//! `Authorization` send: Telegram's `X-Telegram-Bot-Api-Secret-Token` and
//! `RingCentral`'s `Verification-Token`. The company is the route's
//! `{company_id}` path segment.

use axum::extract::{FromRef, FromRequestParts, RawPathParams};
use axum::http::{HeaderMap, StatusCode, request::Parts};
use common::access_token::{self as tokens, CACHE_TTL, Denied, Scope, TokenCache, VerifyError};
use lambda_http::tracing;
use sqlx::MySqlPool;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::libs::constants::{FORBIDDEN_RESPONSE, internal_error};
use crate::libs::types::BasicResponse;
use crate::posthog::{PostHogEvent, client};

/// The tokens this Lambda instance has looked up.
static TOKENS: LazyLock<TokenCache> = LazyLock::new(|| TokenCache::new(CACHE_TTL));

const UNAUTHORIZED_RESPONSE: BasicResponse = (StatusCode::UNAUTHORIZED, "unauthorized");

/// The scope a route needs, as a type: `AccessToken<scope::Leads>`.
pub trait TokenScope {
    const SCOPE: Scope;
}

pub mod scope {
    use super::{Scope, TokenScope};

    pub struct Leads;
    pub struct Internal;
    pub struct Telephony;
    pub struct Telegram;
    pub struct Ses;

    impl TokenScope for Leads {
        const SCOPE: Scope = Scope::Leads;
    }
    impl TokenScope for Internal {
        const SCOPE: Scope = Scope::Internal;
    }
    impl TokenScope for Telephony {
        const SCOPE: Scope = Scope::Telephony;
    }
    impl TokenScope for Telegram {
        const SCOPE: Scope = Scope::Telegram;
    }
    impl TokenScope for Ses {
        const SCOPE: Scope = Scope::Ses;
    }
}

/// What a route's old check made of a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LegacyCheck {
    /// The old secret matched.
    Valid,
    /// Let through without a matching secret, as the route always did
    /// (lead forms, SES, Telnyx).
    Unverified,
    /// Turned away, as the route always did.
    Rejected,
}

impl LegacyCheck {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::Unverified => "unverified",
            Self::Rejected => "rejected",
        }
    }
}

/// A route's check from before access tokens, kept while [`Transitional`].
pub trait LegacyAuth: TokenScope {
    /// Names the old check in `PostHog` events.
    const NAME: &'static str;

    fn check(headers: &HeaderMap) -> LegacyCheck;
}

/// The access token in `headers`, if any looks like one.
fn presented_token(headers: &HeaderMap) -> Option<&str> {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
    };
    let authorization = header("authorization").map(|value| {
        value
            .get(..7)
            .filter(|scheme| scheme.eq_ignore_ascii_case("bearer "))
            .map_or(value, |_| value[7..].trim())
    });
    [
        authorization,
        header("x-telegram-bot-api-secret-token"),
        header("verification-token"),
    ]
    .into_iter()
    .flatten()
    .find(|value| value.starts_with(tokens::TOKEN_PREFIX))
}

/// The route's `{company_id}` path segment. `None` when the route has none
/// (or it is not a number, which the handler's `Path` turns away anyway).
async fn path_company_id<S: Send + Sync>(parts: &mut Parts, state: &S) -> Option<i32> {
    let params = RawPathParams::from_request_parts(parts, state).await.ok()?;
    params
        .iter()
        .find(|(name, _)| *name == "company_id")
        .and_then(|(_, value)| value.parse().ok())
}

/// A request with a working access token for scope `S`.
pub struct AccessToken<S> {
    pub token: tokens::AccessToken,
    scope: PhantomData<fn() -> S>,
}

impl<St, S> FromRequestParts<St> for AccessToken<S>
where
    St: Send + Sync,
    MySqlPool: FromRef<St>,
    S: TokenScope,
{
    type Rejection = BasicResponse;

    async fn from_request_parts(parts: &mut Parts, state: &St) -> Result<Self, Self::Rejection> {
        let company_id = path_company_id(parts, state).await;
        let report = AuthReport::new(parts, S::SCOPE, company_id);
        let Some(token) = presented_token(&parts.headers) else {
            report.token_rejected(Denied::Missing).await;
            return Err(UNAUTHORIZED_RESPONSE);
        };
        let pool = MySqlPool::from_ref(state);
        match TOKENS.verify(&pool, token, S::SCOPE, company_id).await {
            Ok(token) => Ok(Self {
                token,
                scope: PhantomData,
            }),
            Err(VerifyError::Denied(denied)) => {
                report.token_rejected(denied).await;
                Err(if denied.is_forbidden() {
                    FORBIDDEN_RESPONSE
                } else {
                    UNAUTHORIZED_RESPONSE
                })
            }
            Err(VerifyError::Database(error)) => {
                tracing::error!(%error, "Failed to look up an access token");
                Err(internal_error("failed to check the access token"))
            }
        }
    }
}

/// A request let in by a working access token for `L`'s scope, or else by
/// `L`, the route's old check (see the module docs).
pub struct Transitional<L> {
    /// `None` when the old check let the request in.
    pub token: Option<tokens::AccessToken>,
    legacy: PhantomData<fn() -> L>,
}

impl<L> Transitional<L> {
    const fn new(token: Option<tokens::AccessToken>) -> Self {
        Self {
            token,
            legacy: PhantomData,
        }
    }
}

impl<St, L> FromRequestParts<St> for Transitional<L>
where
    St: Send + Sync,
    MySqlPool: FromRef<St>,
    L: LegacyAuth,
{
    type Rejection = BasicResponse;

    async fn from_request_parts(parts: &mut Parts, state: &St) -> Result<Self, Self::Rejection> {
        let company_id = path_company_id(parts, state).await;
        let report = AuthReport::new(parts, L::SCOPE, company_id);
        if let Some(token) = presented_token(&parts.headers) {
            let pool = MySqlPool::from_ref(state);
            match TOKENS.verify(&pool, token, L::SCOPE, company_id).await {
                Ok(token) => return Ok(Self::new(Some(token))),
                // Not refused yet: the old check below still decides, so a
                // caller is never worse off for trying a token.
                Err(VerifyError::Denied(denied)) => report.token_rejected(denied).await,
                Err(VerifyError::Database(error)) => {
                    tracing::error!(%error, "Failed to look up an access token");
                }
            }
        }
        let check = L::check(&parts.headers);
        report.legacy(L::NAME, check).await;
        match check {
            LegacyCheck::Valid | LegacyCheck::Unverified => Ok(Self::new(None)),
            LegacyCheck::Rejected => Err(FORBIDDEN_RESPONSE),
        }
    }
}

/// Requests that did not use a working access token, for `PostHog`.
pub const LEGACY_AUTH_EVENT: &str = "webhook_legacy_auth";
pub const TOKEN_REJECTED_EVENT: &str = "webhook_access_token_rejected";

/// Kept short: a slow `PostHog` must not hold up (or time out) the request.
const REPORT_TIMEOUT: Duration = Duration::from_secs(2);
/// The same event (event, route, company, check and outcome) is sent at most
/// this often per Lambda instance. Enough to see who still has to switch,
/// without an event (and a `PostHog` round trip) per text message.
const REPORT_EVERY: Duration = Duration::from_secs(600);

static LAST_REPORTED: LazyLock<Mutex<HashMap<String, Instant>>> = LazyLock::new(Mutex::default);

/// True when `key` was not reported in the last [`REPORT_EVERY`] (and notes
/// that it is now).
fn due(last: &Mutex<HashMap<String, Instant>>, key: String, now: Instant) -> bool {
    let mut last = last.lock().expect("auth report times");
    if last
        .get(&key)
        .is_some_and(|at| now.duration_since(*at) < REPORT_EVERY)
    {
        return false;
    }
    if last.len() >= 10_000 {
        last.clear();
    }
    last.insert(key, now);
    true
}

/// What is reported about one request. Never the token or the old secret.
struct AuthReport {
    route: String,
    scope: Scope,
    company_id: Option<i32>,
    user_agent: String,
}

impl AuthReport {
    fn new(parts: &Parts, scope: Scope, company_id: Option<i32>) -> Self {
        Self {
            route: crate::posthog::route_pattern(&parts.uri),
            scope,
            company_id,
            user_agent: parts
                .headers
                .get(axum::http::header::USER_AGENT)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect(),
        }
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::from([
            ("route".to_string(), self.route.clone().into()),
            ("scope".to_string(), self.scope.as_str().into()),
            ("company_id".to_string(), self.company_id.into()),
            ("user_agent".to_string(), self.user_agent.clone().into()),
        ])
    }

    fn legacy_properties(
        &self,
        legacy: &str,
        check: LegacyCheck,
    ) -> HashMap<String, serde_json::Value> {
        let mut properties = self.properties();
        properties.insert("legacy".to_string(), legacy.into());
        properties.insert("result".to_string(), check.as_str().into());
        properties
    }

    fn rejected_properties(&self, denied: Denied) -> HashMap<String, serde_json::Value> {
        let mut properties = self.properties();
        properties.insert("reason".to_string(), denied.as_str().into());
        properties
    }

    async fn legacy(&self, legacy: &str, check: LegacyCheck) {
        let key = format!(
            "{LEGACY_AUTH_EVENT}|{}|{:?}|{legacy}|{}",
            self.route,
            self.company_id,
            check.as_str()
        );
        if due(&LAST_REPORTED, key, Instant::now()) {
            send(LEGACY_AUTH_EVENT, self.legacy_properties(legacy, check)).await;
        }
    }

    async fn token_rejected(&self, denied: Denied) {
        tracing::warn!(
            route = %self.route,
            company_id = ?self.company_id,
            reason = denied.as_str(),
            "Access token rejected"
        );
        let key = format!(
            "{TOKEN_REJECTED_EVENT}|{}|{:?}|{}",
            self.route,
            self.company_id,
            denied.as_str()
        );
        if due(&LAST_REPORTED, key, Instant::now()) {
            send(TOKEN_REJECTED_EVENT, self.rejected_properties(denied)).await;
        }
    }
}

async fn send(event: &str, properties: HashMap<String, serde_json::Value>) {
    let Ok(api_key) = std::env::var("POSTHOG_API_KEY") else {
        return;
    };
    let event = PostHogEvent::new_named_event(api_key, event, properties);
    let posthog = client().await;
    match tokio::time::timeout(REPORT_TIMEOUT, posthog.capture(event)).await {
        Ok(Ok(response)) if response.status().is_success() => {}
        Ok(Ok(response)) => tracing::warn!(
            status = response.status().as_u16(),
            "PostHog rejected an auth event"
        ),
        Ok(Err(error)) => tracing::warn!(%error, "Failed to send an auth event"),
        Err(_) => tracing::warn!("Timed out sending an auth event"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderValue, Request};

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    const TOKEN: &str = "gmk_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn token_is_read_from_authorization_with_or_without_bearer() {
        let bearer = format!("Bearer {TOKEN}");
        let lower = format!("bearer  {TOKEN}");
        assert_eq!(
            presented_token(&headers(&[("authorization", &bearer)])),
            Some(TOKEN)
        );
        assert_eq!(
            presented_token(&headers(&[("authorization", &lower)])),
            Some(TOKEN)
        );
        assert_eq!(
            presented_token(&headers(&[("authorization", TOKEN)])),
            Some(TOKEN)
        );
    }

    #[test]
    fn token_is_read_from_the_provider_headers() {
        assert_eq!(
            presented_token(&headers(&[("x-telegram-bot-api-secret-token", TOKEN)])),
            Some(TOKEN)
        );
        assert_eq!(
            presented_token(&headers(&[("verification-token", TOKEN)])),
            Some(TOKEN)
        );
        // The old secret in Authorization, a token in another header.
        assert_eq!(
            presented_token(&headers(&[
                (
                    "authorization",
                    "Bearer 9ca4dfa8-0eec-46cc-967f-3385624be883"
                ),
                ("verification-token", TOKEN),
            ])),
            Some(TOKEN)
        );
    }

    #[test]
    fn old_secrets_are_not_tokens() {
        assert_eq!(
            presented_token(&headers(&[(
                "authorization",
                "Bearer 9ca4dfa8-0eec-46cc-967f-3385624be883"
            )])),
            None
        );
        assert_eq!(
            presented_token(&headers(&[("authorization", "remix-key")])),
            None
        );
        assert_eq!(presented_token(&HeaderMap::new()), None);
    }

    #[tokio::test]
    async fn company_comes_from_the_company_id_path_segment() {
        use axum::{Router, routing::post};
        use axum_test::TestServer;

        async fn handler(mut parts: Parts) -> String {
            format!("{:?}", path_company_id(&mut parts, &()).await)
        }
        let app = Router::new()
            .route("/a/{company_id}/{customer_id}", post(handler))
            .route("/b/{customer_id}", post(handler))
            .route("/c/{company_id}", post(handler));
        let server = TestServer::builder().build(app);
        for (uri, expected) in [("/a/7/9", "Some(7)"), ("/b/7", "None"), ("/c/x", "None")] {
            assert_eq!(server.post(uri).await.text(), expected, "{uri}");
        }
    }

    #[test]
    fn the_same_report_is_sent_at_most_every_ten_minutes() {
        let last = Mutex::default();
        let start = Instant::now();
        assert!(due(&last, "a".to_string(), start));
        assert!(!due(
            &last,
            "a".to_string(),
            start + Duration::from_secs(599)
        ));
        assert!(due(&last, "b".to_string(), start + Duration::from_secs(1)));
        assert!(due(&last, "a".to_string(), start + REPORT_EVERY));
    }

    #[test]
    fn reports_never_carry_the_token() {
        let request = Request::post("/v1/webhooks/new-lead-form/12")
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("user-agent", "Zapier")
            .body(())
            .unwrap();
        let (parts, ()) = request.into_parts();
        let report = AuthReport::new(&parts, Scope::Leads, Some(12));

        let legacy = report.legacy_properties("marketing_uuid", LegacyCheck::Unverified);
        assert_eq!(legacy["route"], "/v1/webhooks/new-lead-form/{id}");
        assert_eq!(legacy["company_id"], 12);
        assert_eq!(legacy["scope"], "leads");
        assert_eq!(legacy["legacy"], "marketing_uuid");
        assert_eq!(legacy["result"], "unverified");
        assert_eq!(legacy["user_agent"], "Zapier");

        let rejected = report.rejected_properties(Denied::Expired);
        assert_eq!(rejected["reason"], "expired");

        for value in legacy.values().chain(rejected.values()) {
            assert!(!value.to_string().contains("gmk_"));
        }
    }
}

#[cfg(test)]
mod route_tests {
    use super::*;
    use crate::axum_helpers::guards::{CORRECT_ID, MarketingUser, RemixBackend};
    use axum::{Router, routing::post};
    use axum_test::TestServer;
    use common::access_token::NewAccessToken;

    async fn company(pool: &MySqlPool, id: i32) {
        sqlx::query("INSERT IGNORE INTO company (id, name) VALUES (?, 'Token Co')")
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
    }

    async fn token(pool: &MySqlPool, company_id: Option<i32>, scopes: &[Scope]) -> (i64, String) {
        if let Some(id) = company_id {
            company(pool, id).await;
        }
        let (id, token) = tokens::create(
            pool,
            &NewAccessToken {
                name: "route test",
                company_id,
                scopes,
                expires_at: None,
                created_by: None,
            },
        )
        .await
        .unwrap();
        (id.try_into().unwrap(), token)
    }

    async fn which<L>(auth: Transitional<L>) -> &'static str {
        if auth.token.is_some() {
            "token"
        } else {
            "legacy"
        }
    }

    fn app(pool: MySqlPool) -> TestServer {
        let router = Router::new()
            .route(
                "/strict/{company_id}",
                post(|_: AccessToken<scope::Leads>| async { "ok" }),
            )
            .route(
                "/strict",
                post(|_: AccessToken<scope::Leads>| async { "ok" }),
            )
            .route("/leads/{company_id}", post(which::<MarketingUser>))
            .route("/internal/{company_id}", post(which::<RemixBackend>))
            .with_state(pool);
        TestServer::builder().build(router)
    }

    #[test_db::test]
    async fn strict_needs_a_working_token(pool: MySqlPool) {
        let (_, own) = token(&pool, Some(901), &[Scope::Leads]).await;
        let (_, internal_only) = token(&pool, None, &[Scope::Internal]).await;
        let (_, internal_leads) = token(&pool, None, &[Scope::Leads]).await;
        let app = app(pool);

        let status = |response: axum_test::TestResponse| response.status_code();
        assert_eq!(
            status(app.post("/strict/901").await),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status(
                app.post("/strict/901")
                    .authorization_bearer(tokens::new_token())
                    .await
            ),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status(
                app.post("/strict/901")
                    .authorization_bearer(CORRECT_ID.to_string())
                    .await
            ),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status(app.post("/strict/901").authorization_bearer(&own).await),
            StatusCode::OK
        );
        assert_eq!(
            status(app.post("/strict/902").authorization_bearer(&own).await),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            status(app.post("/strict").authorization_bearer(&own).await),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            status(
                app.post("/strict/901")
                    .authorization_bearer(&internal_only)
                    .await
            ),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            status(
                app.post("/strict/902")
                    .authorization_bearer(&internal_leads)
                    .await
            ),
            StatusCode::OK
        );
        assert_eq!(
            status(
                app.post("/strict")
                    .authorization_bearer(&internal_leads)
                    .await
            ),
            StatusCode::OK
        );
    }

    #[test_db::test]
    async fn strict_turns_away_a_revoked_token_once_the_cache_expires(pool: MySqlPool) {
        let (id, own) = token(&pool, Some(903), &[Scope::Leads]).await;
        tokens::revoke(&pool, id).await.unwrap();
        // Never looked up before, so nothing cached: refused at once.
        let response = app(pool)
            .post("/strict/903")
            .authorization_bearer(&own)
            .await;
        assert_eq!(response.status_code(), StatusCode::UNAUTHORIZED);
    }

    #[test_db::test]
    async fn transitional_lead_forms_never_turn_anyone_away(pool: MySqlPool) {
        let (_, own) = token(&pool, Some(904), &[Scope::Leads]).await;
        let (revoked_id, revoked) = token(&pool, Some(904), &[Scope::Leads]).await;
        tokens::revoke(&pool, revoked_id).await.unwrap();
        let app = app(pool);

        let text = |response: axum_test::TestResponse| {
            assert_eq!(response.status_code(), StatusCode::OK);
            response.text()
        };
        assert_eq!(
            text(app.post("/leads/904").authorization_bearer(&own).await),
            "token"
        );
        assert_eq!(text(app.post("/leads/904").await), "legacy");
        assert_eq!(
            text(
                app.post("/leads/904")
                    .authorization_bearer(CORRECT_ID.to_string())
                    .await
            ),
            "legacy"
        );
        // A token that does not work here still gets in, as everyone did.
        assert_eq!(
            text(app.post("/leads/905").authorization_bearer(&own).await),
            "legacy"
        );
        assert_eq!(
            text(app.post("/leads/904").authorization_bearer(&revoked).await),
            "legacy"
        );
    }

    #[test_db::test]
    async fn transitional_internal_routes_still_need_the_old_key_or_a_token(pool: MySqlPool) {
        let (_, internal) = token(&pool, None, &[Scope::Internal]).await;
        let (_, leads) = token(&pool, None, &[Scope::Leads]).await;
        let app = app(pool);

        let response = app
            .post("/internal/906")
            .authorization_bearer(&internal)
            .await;
        assert_eq!(response.status_code(), StatusCode::OK);
        assert_eq!(response.text(), "token");
        // REMIX_KEY is not set in tests, so the old check refuses everything.
        assert_eq!(
            app.post("/internal/906").await.status_code(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            app.post("/internal/906")
                .authorization_bearer(&leads)
                .await
                .status_code(),
            StatusCode::FORBIDDEN
        );
    }
}
