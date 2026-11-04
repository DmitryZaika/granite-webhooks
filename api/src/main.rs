use api::app;
use api::db::lambda_pool;
use api::posthog_autofix::Autofix;
use api::state::AppState;
use lambda_http::{Error, run, run_with_streaming_response, tracing};
use std::env::set_var;

#[tokio::main]
async fn main() -> Result<(), Error> {
    // Here, not where it is reported: a new commit then rebuilds only this.
    common::release::set(option_env!("GIT_SHA"));
    tracing::init_default_subscriber();
    common::amazon::secrets::load_into_env().await?;
    unsafe {
        set_var("AWS_LAMBDA_HTTP_IGNORE_STAGE_IN_PATH", "true");
    }
    let url = std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL must be set")?;
    let pool = lambda_pool(url.parse()?);
    // The PostHog auto-fix settings live in a crate above `AppState`'s.
    let state = AppState::from_env(pool)?.with_extension(Autofix::from_env());
    // The same API behind a Function URL in RESPONSE_STREAM mode, for the
    // answers that stream (the AI chat) or outlast API Gateway's 30 s.
    if std::env::var("LAMBDA_RESPONSE_STREAMING").as_deref() == Ok("1") {
        return run_with_streaming_response(app(state)).await;
    }
    run(app(state)).await
}
