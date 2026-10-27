# webhooks — third parties' callbacks Lambda

Lead forms (WordPress, Facebook, Make/Zapier), the Telegram lead bot,
inbound email from SES, the emails and texts employees send, and the
CloudTalk, RingCentral and Telnyx webhooks.

## Layout

The Lambda is one binary built from the `webhooks` crate and the crates in
`crates/`. A change recompiles only its crate and the crates above it, and
the crates compile in parallel. Each crate re-exports the crates below it,
so a module keeps the path it had in the single crate (`crate::crud::deals`,
`crate::axum_helpers::guards::Telegram`) in whichever crate the code is.

```
webhooks/
  src/                  the `webhooks` crate, on top of every other one
    main.rs             Lambda entry
    lib.rs              run(): serves the router on the Lambda runtime
    axum_helpers/axum_app.rs   new_main_app(): every crate's routes, the
                        request logger, CORS
  crates/               bottom to top; a crate may use only the crates below it
    core/   webhooks-core   database code (crud), request schemas, auth
                            extractors and the Telegram bots (axum_helpers),
                            PostHog, OpenAI, the request logger, the
                            providers' payload schemas, the test helpers (tests)
    email/  webhooks-email  inbound email (amazonses), POST /email/send (outbound)
    phone/  webhooks-phone  CloudTalk, RingCentral, Telnyx, POST /sms/send
                            (outbound::sms), Google address lookups
    leads/  webhooks-leads  lead forms (webhooks), the Telegram bot (telegram),
                            lead and referral logic (libs), templates (template),
                            the lead form route's OpenAPI document (openapi)
  openapi.json          the lead form route's spec, served at /api-docs/openapi.json
```

`core -> email, phone -> leads (phone) -> webhooks`. Each crate's routes are
in its `lib.rs` (`router()`). New code goes in the crate of its area; code
two crates need moves down into `core`.

## Checks and tests while you work

- `cargo clippy -p webhooks-<crate> --all-targets` for the crate you edited.
  It type-checks too, so do not also run `cargo check`.
- `cargo test -p webhooks-<crate> <module>::` for one module's tests.
- After changing what other crates use (a signature, a type in `core`):
  `cargo clippy -p 'webhooks*' --all-targets`.
- Before you push: `cargo test -p 'webhooks*'` (every webhooks test).

Tests are unit tests beside the code. The helpers are in
`crates/core/src/tests/` (`crate::tests::...` in every crate):
`#[test_db::test]` gives a test its own database, `crate::tests::utils`
inserts rows and serves the crate's own routes (`new_test_app`), and
`crate::tests::stub_http` stands in for CloudTalk, Google and the api. The
other crates' tests get them through `webhooks-core`'s `test-support`
feature, which their dev-dependencies turn on; it also makes the code's test
stand-ins (stub URLs, placeholder bot tokens) apply when another crate's
tests call it. The Lambda is never built with it.

`make webhooks-openapi` regenerates `openapi.json` after a change to the lead
form route's annotations (utoipa only runs with webhooks-leads' `openapi`
feature).
