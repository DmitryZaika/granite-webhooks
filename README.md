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

Every webhooks route is moving to one kind of credential: an access token
(`gmk_` + 43 characters) in `Authorization: Bearer <token>`. Telegram sends it
as its `secret_token` (`X-Telegram-Bot-Api-Secret-Token`) and RingCentral as
its subscription `verificationToken` (`Verification-Token`).

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

While callers switch, routes use `Transitional<...>`
(`webhooks/src/axum_helpers/access_token.rs`): a working token gets in, and
otherwise the route's old check decides exactly as before (lead forms, SES and
Telnyx still let everyone in). PostHog shows who has not switched:

- `webhook_legacy_auth`: a request let in (or refused) by the old check, with
  `route`, `company_id`, `legacy` and `result`.
- `webhook_access_token_rejected`: a token that did not work, with `reason`.

Each distinct event is sent at most every 10 minutes per Lambda instance.
When a route's `webhook_legacy_auth` events stop, switch it to
`AccessToken<scope::...>` (401/403 without a working token) and delete its old
check from `guards.rs`.

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
