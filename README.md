# Introduction

grante-webhooks is a Rust project that implements an AWS Lambda function in Rust.

## Migrations

mysqldump -u root -p test --no-data > schema_only.sql

## Deploying

uvx cargo-lambda lambda build --release
uvx cargo-lambda lambda deploy --iam-role arn:aws:iam::741448943665:role/cargo-lambda-role-2ed5069c-8882-460d-bdc8-192d9b724756

## Testing

uvx cargo-lambda lambda watch --env-file .env


Connect to internet: ngrok http 9000

Conntect to telegram:

```
curl -X POST "https://api.telegram.org/bot<token>/setWebhook" \
      -d "url=<ngrok>/telegram/webhook" \
      -d "secret_token=fake"

||

set-dev-telegram https://d608ecb7196d.ngrok-free.app
```

When server dies instantly:

1. Find server: sudo lsof -i :9000
2. Kill that server: kill -9 <PID>

## Access tokens

Every webhooks route takes one kind of credential: an access token
(`gmk_` + 43 characters) in `Authorization: Bearer <token>`. Telegram sends it
as its `secret_token` (`X-Telegram-Bot-Api-Secret-Token`), RingCentral as
its subscription `verificationToken` (`Verification-Token`), and Telnyx, which
can set no header on its TeXML callbacks, in the callback URL
(`/telnyx/event-progress?token=<token>`, that route only).

Every route needs a working token (`AccessToken<...>`, 401 without one, 403
for a token that may not call it), except `GET /` (health),
`GET /api-docs/openapi.json` (the lead form's public spec) and the lead forms.
The old secrets (`REMIX_KEY`, `WEBHOOK_SECRET`, the shared bearer UUID) no
longer work anywhere.

Tokens live in `access_tokens` (only their SHA-256). Each has scopes
(`leads`, `internal`, `telephony`, `telegram`, `ses`) and either one company,
which must match the route's `{company_id}`, or none (internal: every
company). The Lambda caches a token for 60 seconds, so a revocation takes
effect within a minute.

```
make access-token ARGS="create --name 'Acme Zapier' --scopes leads --company-id 12 --expires 2027-10-25"
make access-token ARGS="create --name 'api Lambda' --scopes internal,leads,telephony --internal --no-expiry"
make access-token ARGS="list"
make access-token ARGS="revoke 7"
```

Cargo pins `DATABASE_URL` to the local MySQL, so set
`ACCESS_TOKEN_DATABASE_URL` to create tokens in the dev or production
database. The token is printed once. Tokens are letters and digits only:
`RingCentral` refuses a `-` in a subscription's `verificationToken`.

The `api` Lambda uses one token for everything (`WEBHOOKS_KEY`, see
`api/README.md`): its calls here, and the `verificationToken` of the
`RingCentral` SMS subscriptions it makes.

The lead forms (`/wordpress-contact-form`, `/facebook-contact-form`,
`/v1/webhooks/new-lead-form`) use `Transitional<MarketingUser>`
(`webhooks/crates/core/src/axum_helpers/access_token.rs`) so that no lead is
ever lost: everyone still gets in, and PostHog shows who sends no token yet:

- `webhook_legacy_auth`: a lead let in without a working token, with `route`,
  `company_id`, `legacy` and `result`.
- `webhook_access_token_rejected`: a token that did not work (on any route),
  with `reason`.

Each distinct event is sent at most every 10 minutes per Lambda instance.
When the lead forms' `webhook_legacy_auth` events stop, switch them to
`AccessToken<scope::Leads>` too.

## Build cache

Each checkout or worktree has its own `target/` and would build the ~400
dependencies from scratch. With [sccache](https://github.com/mozilla/sccache)
as cargo's compiler wrapper, a dependency compiled once (same version,
features and flags) comes from the shared cache in every other worktree:

```
brew install sccache
printf '[build]\nrustc-wrapper = "sccache"\n' >> ~/.cargo/config.toml
```

sccache hashes absolute paths, and every worktree's `target/` has its own, so
list the worktree roots as base directories in
`~/Library/Application Support/Mozilla.sccache/config` (macOS), adding each
new worktree:

```toml
basedirs = ["/Users/you/granite-webhooks", "/Users/you/granite-webhooks-ci"]
```

`sccache --show-stats` shows the hits. It is set per machine, not in
`.cargo/config.toml`, so CI and machines without sccache are unaffected.
Workspace crates (incremental), proc macros and build scripts are not cached.

### macOS

macOS checks every new program before it first runs (Gatekeeper and
XProtect), and each build makes new test binaries: a big one waits a few
seconds, about half of a `cargo test -p 'api*'` after a change. Programs
started by an app listed under Developer Tools are not checked. Run once:

```
sudo spctl developer-mode enable-terminal
```

It lists Terminal in System Settings > Privacy & Security > Developer Tools.
Turn on there each app that runs cargo (add another terminal, such as
Alacritty or iTerm2, or an editor with +), then restart it.

### Windows

The same commands work. What makes the builds slower there, and helps:

- Linking: MSVC's `link.exe` is slow on big test binaries. Rust ships
  `rust-lld`; to use it, add to `%USERPROFILE%\.cargo\config.toml`
  (`aarch64-pc-windows-msvc` on an ARM machine):

  ```toml
  [target.x86_64-pc-windows-msvc]
  linker = "rust-lld.exe"
  ```

- Microsoft Defender scans every file cargo writes: exclude the checkout's
  `target\` folder (Windows Security, Virus & threat protection, Exclusions).
- `make test`/`cargo test` need the local MySQL (see `make api-db-up`):
  `common`'s `query!` macros check their SQL against it at compile time, and
  the tests make their databases on it.

## Functions

### Documenso

```
curl -v -X POST \
  'http://127.0.0.1:9000/lambda-url/granite-webhooks/documenso' \
  -H 'Content-Type: application/json' \
  -d '{ "event": "DOCUMENT_CREATED" }'

```

###

## API (frontend-facing Lambda)

The `api/` crate replaces Remix loaders with JSON endpoints. See `api/README.md`
for the route layout, auth, local setup (`make api-db-reset api-test api-local`)
and the migration playbook.

## Webhooks (third parties' callbacks Lambda)

The `webhooks/` crate and the crates in `webhooks/crates/`. See
`webhooks/README.md` for which crate holds what and the checks to run while
coding (`cargo test -p 'webhooks*'`).
