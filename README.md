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

### Self-serve billing env

Every variable the self-serve billing feature reads (Stripe, signup, advance
payment, billing emails). The api checks them at start: an enabled switch with a
missing dependency, or a malformed value, stops the Lambda with a message that
names the variable (never its value). `GET /v1/superuser/billing/setup` reports
the same and the Stripe side. The release steps are in
`general_datebase/docs/self-serve-release.md`.

`granite-api` (and `granite-api-stream`, same image):

| Name | Required when | Default |
|---|---|---|
| `SESSION_SECRET` | always (also keys the signup email codes) | none: the api does not start |
| `STRIPE_SECRET_KEY` | any billing; `sk_`/`rk_`, a restricted live key in production | unset: Stripe off |
| `STRIPE_PUBLIC_KEY` | signups and payment method pages; `pk_`, same mode as the secret key | none |
| `STRIPE_WEBHOOK_SECRET` | Stripe webhooks; `whsec_`, from the setup script's secret file | unset: webhooks refused |
| `STRIPE_BILLING_PLANS` / `STRIPE_PRICE_ID` | legacy (grandfathered) plans only | none |
| `STRIPE_AUTOMATIC_TAX` | only when tax is turned on (`1`/`true`) | off |
| `STRIPE_API_BASE` | tests and e2e only; ignored with a live key | `https://api.stripe.com` |
| `SIGNUP_TEST_PAYMENT_METHOD` | sandbox e2e only; ignored unless the key is `sk_test_` | none |
| `SIGNUP_RATE_LIMITS` | e2e only: `off` lifts the per-email and per-IP signup limits; ignored unless the key is `sk_test_` | limits on |
| `SELF_SERVE_SIGNUP_ENABLED` | launch switch (`1`/`true`); needs the three Stripe keys and `WEBHOOKS_URL`/`WEBHOOKS_KEY` | off |
| `ONBOARDING_ADVANCE_CHARGING` | charges the advance payment (`1`/`true`); needs `STRIPE_SECRET_KEY` and `CALENDLY_API_TOKEN` | off: a passed deadline ends `advance_skipped` |
| `CALENDLY_API_TOKEN` | the onboarding call step and the deadline tolling | none |
| `CALENDLY_BASE_URL` | tests only | `https://api.calendly.com` |
| `CALENDLY_ONBOARDING_URL` | optional; a calendly.com page | the account's own page |
| `WEBHOOKS_URL`, `WEBHOOKS_KEY` | every billing email; both or neither | unset: emails not sent |
| `BILLING_EMAIL_FROM_ADDRESS` | optional; a plain address | `noreply@granite-manager.com` |
| `BILLING_ALERT_EMAIL` | staff notices: disputes, blocked bank accounts, bank returns, advance notices | none: notices dropped |
| `SIGNUP_NOTIFY_USER_ID`, `SIGNUP_NOTIFY_EMAIL` | optional staff alerts (Telegram user id, email) for signups and stuck provisioning | none |
| `APP_URL` | optional | `https://granite-manager.com` |
| `PAYMENTS_APP_URL` | optional | `https://payments.granite-manager.com` |
| `SESSION_COOKIE_DOMAIN`, `SESSION_COOKIE_SECURE` | optional; the payments app shares the CRM cookie | `.granite-manager.com`, Secure |
| `API_ALLOWED_ORIGINS` | optional; must include the payments origin when set | `https://granite-manager.com,https://*.granite-manager.com` |
| `LAMBDA_KEY` | the scheduled jobs; same value as on `time-triggered` | none: jobs refused |

`time-triggered`: `API_URL`, `API_STREAM_URL` and `LAMBDA_KEY` (unchanged). It
calls the billing jobs `subscription-maintenance`, `billing-reminders` and
`onboarding-commitments` every tick; its Lambda timeout must be at least 7 minutes.

Front-end build values (`VITE_SELF_SERVE_ENABLED`, `VITE_PAYMENTS_URL`, the
payments app's `VITE_*`) are in the general_datebase README.

## Webhooks (third parties' callbacks Lambda)

The `webhooks/` crate and the crates in `webhooks/crates/`. See
`webhooks/README.md` for which crate holds what and the checks to run while
coding (`cargo test -p 'webhooks*'`).
