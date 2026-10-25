# api — frontend-facing Lambda

This crate replaces the Remix loaders/actions in `general_datebase` with JSON
endpoints, one route at a time, so the frontend can eventually be a static
React app. The twin of this document in the frontend repo is
`docs/backend-migration.md`.

## Layout

```
api/
  src/
    main.rs              Lambda entry (lambda_http + axum)
    lib.rs               app(state) -> Router; used by main and by tests
    openapi.rs           OpenAPI document info, security scheme, tags
    state.rs             AppState (pool, SESSION_SECRET, CORS origins)
    error.rs             ApiError -> JSON {"error": "..."} with 400/401/403/500
    extract.rs           MultiQuery: query strings with repeated keys (list filters)
    sql.rs               QueryBuilder helpers (push_in: ` AND col IN (...)`)
    auth/
      cookie.rs          React Router `__session` cookie decode/encode
      session.rs         session -> user lookup, super-admin company switch
      mod.rs             extractors: CurrentUser, EmployeeUser, AdminUser
    routes/
      mod.rs             merges every domain router
      <domain>/
        mod.rs           router() registering handlers with routes! + a doc list of the routes
        handlers.rs      #[utoipa::path] + auth extractor + params -> query -> Json
        queries.rs       SQL, one function per database read
        schemas.rs       request params, response rows (serde + FromRow + ToSchema)
  tests/
    common/mod.rs        test server, signed cookies, seeded session ids
    <domain>.rs          integration tests per domain
    openapi.rs           every route documented, openapi.json current (no DB)
  openapi.json           generated spec, checked in (`make api-openapi`)
  seed/local_seed.sql    deterministic data for tests and local runs
  examples/sign_session.rs   print a Cookie header for curl
```

### Domains and where routes go

One folder under `routes/` per business area. Name it after the resource, not
the Remix page that uses it. Pick the folder by the main table the query reads:

| Domain folder   | Paths                     | Main tables                                    |
|-----------------|---------------------------|------------------------------------------------|
| `me`            | `/v1/me/...`              | `users`, `users_positions` for the session user |
| `customers`     | `/v1/customers/...`       | `customers`, `customers_emails`                |
| `deals`         | `/v1/deals/...`           | `deals`, `deals_list`, `groups_list`           |
| `sales`         | `/v1/sales/...`           | `sales`, `slab_inventory` when tied to a sale  |
| `stones`        | `/v1/stones/...`          | `stones`, `slab_inventory`, `stone_images`     |
| `sinks` / `faucets` / `supplies` | `/v1/sinks/...` etc. | the matching inventory tables     |
| `users`         | `/v1/users/...`           | other users of the company (admin screens)     |
| `schedule`      | `/v1/events/...`          | `events`, calendars                            |
| `emails` / `sms`| `/v1/emails/...`          | message tables                                 |

Exist so far: `me`, `customers`, `users`, `stones`, `slabs`, `sinks`, `faucets`
(sinks and faucets share `routes/fixtures.rs`), `sales`, `catalog` (suppliers,
colors, stone types), `checklists` (post-installation checklists), `uploads`
and `public` (the showroom, no session).
Create the others as their first route is migrated, following the same files.

### Public routes

`/v1/public/...` take no session (no extractor): `tests/openapi.rs` exempts
them from `security` and 401. They must never return costs, hidden items or
other companies' data; `tests/public.rs` checks that.

`/v1/webhooks/...` are third parties' callbacks (Stripe, SignWell): no
session either, the sender proves itself (`Stripe-Signature` with
`STRIPE_WEBHOOK_SECRET`; SignWell's `?token=` = `SIGNWELL_WEBHOOK_SECRET` or
its event hash keyed with `SIGNWELL_WEBHOOK_ID`).
Point the provider's webhook at `https://api.granite-manager.com/v1/webhooks/<name>`.

### Streaming answers

The AI chat (`POST /v1/ai/chat`, server-sent events) and other long AI
calls outlast API Gateway's 30 s. The same binary also runs behind a Lambda
Function URL in `RESPONSE_STREAM` mode: set `LAMBDA_RESPONSE_STREAMING=1` on
that function (`main.rs` then uses `run_with_streaming_response`). Put it on
a subdomain of the session cookie's domain (for example through CloudFront)
so the browser sends the cookie; the frontend calls it from
`VITE_GRANITE_STREAM_URL`. The chat's CRM manual is
`api/src/chat/crm_manual.json`, rebuilt from the CRM repo with
`node scripts/sync-crm-manual.mjs ../general_datebase`.

### Pictures, PDFs and audio

Pure Rust, so nothing to install on Lambda: `images.rs` reads phone photos
(HEIC with `zune-heic`) and makes thumbnails and SMS-ready pictures;
`pdf_preview.rs` draws a PDF's first page with `hayro`. Responses stay
under Lambda's 6 MB: big files go up with presigned PUTs (`POST
/v1/uploads`) and down with presigned GETs or redirects (document and
attachment previews, call recordings copied to the bucket, quote prints).
The bucket's CORS must allow `GET` and `HEAD` too: the call player fetches
the recording. `CLOUDTALK_BASE_URL` points CloudTalk calls at a stand-in
(the frontend's browser tests).

Signed documents (`routes/signatures.rs`) open in a browser tab with the
session cookie. Native ones are read from the private
`SIGNATURE_STORAGE_BUCKET` (same storage credentials) and hash-checked;
SignWell signed copies are fetched and titled "<document> signed". PDFs over
4 MB redirect to a presigned link (big SignWell copies are stored under
`signed-documents/`, which a bucket lifecycle rule may expire after a day).

Native signatures are on with `NATIVE_SIGNATURES_ENABLED=1`,
`SIGNATURE_INVITATION_SECRET` (32 bytes, base64url) and
`SIGNATURE_APP_ORIGIN` (`native_signing.rs`); the scheduled
`POST /v1/jobs/signature-reminders` then expires open requests and sends
reminders and notifications (it does nothing while they are off).

Sending documents for signature (`POST /v1/emails` with
`signature_requests`, `routes/emails/signatures.rs`): through SignWell by
default (the company's `signwell_api_key`, else `SIGNWELL_API_KEY`;
`SIGNWELL_TEST_MODE=true` for test documents), natively when enabled. A
native send also needs the company's verified settings in
`SIGNATURE_VERIFIED_COMPANY_SETTINGS` (`{"<company id>": {"verifiedAt",
"evidenceReference", "settings"}}`) and a `SIGNATURE_STORAGE_BUCKET` that
blocks all public access. `signing_pdf.rs` freezes the PDF's form and prints
what the sender filled in, matching pdf-lib's output.

SignWell reports viewed / signed / declined documents to
`POST /v1/webhooks/signwell` (`routes/signwell_webhook.rs`): the sender is
emailed (links to `SIGNATURE_APP_ORIGIN`) and notified in the app, and a
signed contract ("<contract form> - <quote name>.pdf") sells its quote: the
deal is won with the sold option's total. `quote_pricing/` prices quotes as
the CRM does (`priceQuote`, the price snapshot, stock sync and extras),
checked line by line against the TypeScript on the Moraware fixtures.

### SMS / email flows

`followups/processor.rs` sends the due steps of flows (`sms_flow_enrollments`)
from the scheduled `POST /v1/jobs/sms-followups`, which also sends lead-list
drip texts and everyone's scheduled emails and texts; a rep's start or "send
now" sends the first step before the response. After an outbound call the
`webhooks` Lambda posts to `POST /v1/followups/call-check` (with
`LAMBDA_KEY`; it needs `API_URL` set to this API). Follow-up texts reuse
`sms-flow-<run>-<step>` / `sms-drip-<row>` as the provider's idempotency key.

### Billing (Stripe)

`src/billing/` keeps `company_billing` in step with Stripe (the state machine
is a port of the CRM's `stripeBilling.ts`, tested in memory in
`billing/machine.rs`, against MySQL in `tests/billing.rs` with a fake Stripe,
and against Stripe's test mode in `tests/billing_stripe_live.rs` when
`STRIPE_TEST_SECRET_KEY=sk_test_...` is set). Env: `STRIPE_SECRET_KEY`,
`STRIPE_WEBHOOK_SECRET`, `STRIPE_BILLING_PLANS` (`price|Name|cents,...`) or
`STRIPE_PRICE_ID`.

### Uploads and storage

Images go straight from the browser to the bucket with a presigned PUT
(`POST /v1/uploads`) under `<folder>/<company id>/`; routes that take an
image URL call `routes::uploads::attach` (checks the file is the company's
upload in the folder, or an older upload whose key names no company, and
stores the `-icon` thumbnail) and `remove` when replacing or deleting.
Bucket URLs are public, so `remove` (and anything that overwrites a file)
touches only keys under the caller's company; older keys are never deleted.
Stored URLs never reach `SIGNATURE_STORAGE_BUCKET`: `readable_url` does not
sign it and `get_url` does not read it (`get_in` does, for signature code). `Storage` is
S3 (`STORAGE_BUCKET`, `STORAGE_REGION`, `STORAGE_ACCESS_KEY`,
`STORAGE_SECRET`, optional `STORAGE_ENDPOINT` / `STORAGE_PUBLIC_URL`),
`Storage::memory()` in tests (`common::server_with_storage`), or disabled
(upload routes answer 503). Locally `docker-compose.local.yml` runs
`adobe/s3mock` on :9090. Production needs bucket CORS allowing `PUT` from
`https://*.granite-manager.com`.

### Writes

Multi-statement writes run in one transaction (`state.pool.begin()`), so a
failure leaves nothing half saved. Validate everything that can be checked
before the first write, scope every id the client sends by company, and
answer 400 with a message the page can show.

## Rules for every migrated endpoint

1. **One endpoint per database read.** If a Remix loader runs three queries,
   write three endpoints. The frontend calls them (in parallel where possible)
   and composes the page. Don't add "page" endpoints that bundle reads.
2. **Same results as Remix, not necessarily the same SQL or shape.** Start
   from the old query and keep what it returns (rows, filters, counts); clean
   up the SQL and the JSON shape where that makes a better resource (see
   [Designing a route](#designing-a-route)), and adapt in the frontend's
   `app/lib/api/<domain>.ts`. Use runtime `sqlx::query_as` / `QueryBuilder`
   with `#[derive(FromRow)]` (no compile-time macros), so building this crate
   never needs a database.
3. **Always scope by `user.company_id`** from the auth extractor, including
   lookups by ids the client sends (Remix trusted ids it had just queried
   itself; the API cannot).
4. **Keep the composition in the frontend.** The pure "rows -> page data"
   code (sorting for display, grouping, merging two endpoints) moves from the
   Remix loader into the page or a client-safe module.
5. **Match wire types.** Remix sent mysql2 values: `Date` for timestamps,
   strings for `DECIMAL`, numbers for `COUNT`. The API sends ISO strings
   (`serde_helpers::js_date`), `CAST(... AS CHAR)` for decimals, `i64` for counts,
   `bool` for flags; the frontend adapter converts where the page needs the
   old type (e.g. ISO strings back to `Date`).
6. **Errors become status codes.** Remix's `selectMany` swallowed SQL errors
   and returned `[]`; the API returns 500. Invalid params return 400.
7. **Tests are the proof.** The old loader is deleted in the same change, so
   `tests/<domain>.rs` must cover every branch of the old SQL against seeded
   data: exact JSON for at least one row per response shape, each filter,
   NULL/deleted rows, tenant isolation, and the auth cases. The page itself is
   pinned by Playwright tests in the frontend repo (`e2e/`), written and run
   against the Remix page before migrating it.
8. **Document it for agents.** AI agents call this API from the OpenAPI spec
   alone, so every route is described well enough to use without reading
   the code. See [OpenAPI](#openapi) below; `tests/openapi.rs` fails on
   anything missing.

### Steps to migrate a route

1. Read the Remix loader; list each database read and the permission check.
2. Add rows to `seed/local_seed.sql` (append only; ids >= 100) covering each
   branch of the query (filters, NULLs, deleted rows, a second company).
3. For each read: add a function in `queries.rs`, a handler with
   `#[utoipa::path]`, `.routes(routes!(handlers::name))` in the domain
   `mod.rs`, and tests. `make api-openapi`, then `make api-test`.
4. Frontend: add typed callers in `app/lib/api/<domain>.ts`, fetch them with
   React Query in the page, delete the route's `loader`, and prove the page
   unchanged with the Playwright suite. See `docs/backend-migration.md` in
   that repo; it is the full runbook.

## Designing a route

The Remix loaders were written page by page; the API is designed resource by
resource. Keep the behaviour (tests prove it), not the shape:

- **Resources, not pages.** `GET /v1/stones`, not `GET /v1/employee-stones-page`.
  Unrelated reads a loader bundled become separate endpoints.
- **API names for params.** `include_sold_out=true` (not the page's
  `show_sold_out`), `supplier_id` (not `supplier`). The frontend maps its URL
  filters to these (`stonesListParams` in `app/lib/api/stones.ts`).
- **Lists as repeated keys.** `?type=granite&type=quartz&color_id=2`. Take them
  with `MultiQuery<T>` (`Vec<String>` / `Vec<i32>` fields with
  `#[serde(default)]`); `axum::extract::Query` cannot. Malformed values are a
  JSON 400 (`invalid query string: ...`). Use `#[param(rename = "type")]`
  next to `#[serde(rename = "type")]` so the spec shows the wire name.
- **Honest types.** Flags are booleans (`COALESCE(col, 0) AS col` decodes as
  `bool`), `DECIMAL` is a string (`CAST(col AS CHAR)`), counts are `i64`, and
  related numbers can be grouped (`Stone.slabs` via `#[sqlx(flatten)]`).
- **Deterministic order.** Always end `ORDER BY` with the primary key.

## Seed data

`seed/local_seed.sql` is shared by the Rust tests, the frontend's Playwright
tests (`e2e/` in general_datebase, which reset the docker DB with
`make api-db-reset`) and local dev. Append only; ids by range:

| Range       | Rows                                                        |
|-------------|-------------------------------------------------------------|
| 100-106     | companies 100/101, users, sessions `aaaaaaaa-...-000000000<user id>` |
| 300-302     | suppliers                                                   |
| 1000+, 2000 | customers (2000 = company 101)                              |
| 3000-3009, 4000 | stones (4000 = company 101)                             |
| 3100-3107   | sink types (3106 = company 101)                             |
| 3200-3205   | faucet types (3205 = company 101)                           |
| 5000+       | customer emails                                             |
| 6000+       | slabs; 6100+ sink units; 6200+ faucet units                 |
| 7000-7003, 7010-7012 | sales (7010-7012 for the slabs dialog / sale forms)  |
| 3010, 6070-6077 | stone Taj Mahal (hidden) and its slabs                  |
| 6109-6111   | sink units installed on slab 6071                           |
| 100-104     | stone types; installed photos of stones, sinks and faucets  |
| 9000-9006   | post-installation checklists (9006 = company 101)           |

Changing an existing row breaks assertions in both repos; add a new row.

## OpenAPI

The spec is generated from the code with `utoipa` and served unauthenticated
at `GET /openapi.json`; the same document is checked in as `api/openapi.json`.
AI agents turn each operation into a tool: `operationId` becomes the tool
name, `summary` + `description` its description, and the parameter and schema
descriptions tell it what to send and what comes back.

For every route:

- **Handler**: `#[utoipa::path(...)]` with the method, full `path`, an explicit
  verb_noun `operation_id` (`list_customers`, not `list`), `tag` = the domain,
  `params(...)` / `request_body = ...`, every status the handler can return
  (`body = ErrorBody` for errors; always 401, 403 when it takes
  `EmployeeUser`/`AdminUser`, 400 when it validates input, 500 when it
  queries), and `security(("session_cookie" = []))`.
- **Doc comment on the handler**: first line is the summary (imperative,
  e.g. "List the company's customers"); the paragraph after it says what is
  returned, which filters apply, what is hidden by default, and which other
  route to call next.
- **Schemas**: request/response types derive `ToSchema`, query structs derive
  `IntoParams` with `#[into_params(parameter_in = Query)]`. Every type and
  every field gets a doc comment saying what it means in business terms,
  units/format, and when it is null. When a param is parsed from a string,
  set `#[param(value_type = ...)]` to the type the client should send, and
  add `inline` when that type is not a body schema.
- **Router**: `.routes(routes!(handlers::name))`, never `.route(...)`, or the
  route is served but missing from the spec.
- Run `make api-openapi` to regenerate `api/openapi.json` and commit it.

`tests/openapi.rs` checks all of this (descriptions, operation ids, tags,
security, 401s, unresolved `$ref`s, plain `.route(` calls) and that
`api/openapi.json` is current.

## Auth

The Remix app keeps sessions in the `sessions` table and puts the session id
in the `__session` cookie, which React Router signs with `SESSION_SECRET`:

```
__session = urlencode( base64(json) + "." + base64_nopad(HMAC-SHA256(SESSION_SECRET, base64(json))) )
json      = {"sessionId": "<uuid>", "activeCompanyId": 123, ...}
```

`auth/cookie.rs` verifies the signature and decodes the JSON, byte-compatible
with react-router 7 (unit-tested against cookies React Router produced).
`auth/session.rs` then runs the same session/user query as Remix `getUser`
and applies the super-admin rule from `handlePermissions`. So the Lambda needs
`SESSION_SECRET` set to the same value as the Remix app.

How the browser sends the cookie: the cookie is `HttpOnly`, and in production
its domain is `.granite-manager.com`. The frontend calls the API with
`credentials: 'include'`, so the browser attaches it as long as the API is on
a `*.granite-manager.com` host (e.g. `api.granite-manager.com`). CORS echoes
allowed origins (`API_ALLOWED_ORIGINS`, default `https://granite-manager.com`
and `https://*.granite-manager.com`) with `Access-Control-Allow-Credentials`.
Locally, cookies ignore ports, so the `localhost:5173` cookie also reaches
`localhost:9100`.

**When Remix goes away:** move login/logout here (`POST /v1/auth/login`
creates the `sessions` row and sets the cookie with `cookie::encode_session`,
`POST /v1/auth/logout` sets `is_deleted = 1`). Because the format is identical,
nobody is logged out at the cutover. After that the cookie can be simplified
(e.g. plain session id, still `HttpOnly; Secure; SameSite=Lax`), since only
Rust reads it. Don't move the session into `localStorage`/bearer tokens: the
`HttpOnly` cookie keeps it out of reach of injected scripts.

## Running locally

```
make api-db-reset   # docker MySQL on :3307, all migrations, seed data
make api-test       # unit + integration tests (sqlx::test makes a DB per test)
make api-local      # Lambda emulator on http://localhost:9100/lambda-url/api
```

`make api-local` reads `api/.env.local` (copied from `.env.local.example`).

Call it with a seeded session (user 100 is `rep@acme.test` in company 100):

```
C=$(cargo run -q -p api --example sign_session -- aaaaaaaa-0000-4000-8000-000000000100)
curl -H "Cookie: $C" http://localhost:9100/lambda-url/api/v1/customers
curl -H "Cookie: $C" -H 'content-type: application/json' \
  -d '{"customer_ids":[1000]}' http://localhost:9100/lambda-url/api/v1/customers/emails/batch
```

## Deploying

```
make build <api|webhooks|time-triggered>
make deploy <api|webhooks|time-triggered> <dev|prod>
```

`make deploy` builds the binary and uploads it to the Lambda for that app and
environment (`granite-api`, `granite-api-dev`, `granite-webhooks`,
`granite-webhooks-dev`, `time-triggered`; there is no dev time-triggered). It
creates the Lambda when it does not exist and never changes its environment
variables.

Production `granite-api` needs `DATABASE_URL`, `SESSION_SECRET` (same as
Remix), optionally `API_ALLOWED_ORIGINS`, and is served on
`api.granite-manager.com` (CloudFront in front of the function URL) so the
session cookie is sent.

## Streaming deployment

`granite-api-stream` runs the same binary with `LAMBDA_RESPONSE_STREAMING=1`
behind a `RESPONSE_STREAM` function URL (15 min timeout, 1 GB). `make deploy api
prod` updates it with `granite-api`; its environment is `granite-api`'s plus
that flag, so copy any new variable to both. The browser reaches it at
`https://stream.granite-manager.com` (`infra/api-domain.yaml`, stack
`granite-api-stream`, us-east-1; CloudFront waits up to 60 s between bytes).
`time-triggered` starts the supplier files job on its function URL directly
(`API_STREAM_URL`), so a long run is not cut by CloudFront.

## Scheduled jobs and webhooks

`time-triggered` (every minute) posts to `<API_URL>/v1/jobs/*` with
`Authorization: <LAMBDA_KEY>`, the API's `LAMBDA_KEY`. `webhooks` asks the API
whether a called customer answered (`<API_URL>/v1/followups/call-check`, same
key) and, after storing an inbound email, has it pushed to the recipients'
phones (`<API_URL>/v1/emails/inbound-push`, same key); the API sends mail and texts through `webhooks` (`WEBHOOKS_URL`, and
`WEBHOOKS_KEY`).

`WEBHOOKS_KEY` is the API's one credential for `webhooks`: an access token
with no company and the scopes `internal,leads,telephony` (see "Access tokens"
in the root `README.md`):

```
make access-token ARGS="create --name 'api Lambda' --scopes internal,leads,telephony --internal --no-expiry"
```

It is sent as `Authorization` on every call, and it is also the
`verificationToken` of the companies' `RingCentral` SMS subscriptions
(`RingCentral` sends it back on each push to `/ringcentral/notify`). Until
`WEBHOOKS_KEY` is an access token it holds the webhooks Lambda's `REMIX_KEY`,
and subscriptions get the older `RINGCENTRAL_WEBHOOK_TOKEN` instead. After a
change of `WEBHOOKS_KEY`, `POST /v1/jobs/contact-sync` renews every
subscription with the new token, two companies a minute; a failed renewal is
tried again 2 hours later. Subscriptions are only made where `LAMBDA_URL` (the
webhooks Lambda) is set: not on dev, whose database holds production's
`RingCentral` credentials.

## Phone (Telnyx)

Granite Phone, the business calling app (repo `granite-phone`; its contract
is `docs/api-contract.md` there), runs on this API. Calls go through Telnyx:
a company's numbers sit on its TeXML application (incoming calls), and each
signed-in phone gets a telephony credential on its credential connection
(outgoing calls, parked until we approve them). Code: `auth/device.rs` (the
app's `X-Granite-Device` token), `telnyx.rs`, `push.rs` (FCM), `phone/`
(shared logic), `routes/device_sessions.rs`, `routes/phone/`,
`routes/telnyx_webhooks/` and `routes/jobs/phone.rs`.

| Routes | For |
|---|---|
| `POST /v1/auth/device-sessions`, `DELETE /v1/auth/device-sessions/current`, `/v1/me/devices*` | the app's sign-in and signed-in phones (device token; `/v1/me/devices` takes the cookie too) |
| `/v1/phone/*` | the app (device token): bootstrap, devices and login JWTs, settings, contacts, Recents, notes, Call via my cell, 911 alerts, problem reports, the Texts tab (`/v1/phone/texts*`, `phone/texts.rs`) |
| `/v1/emails*`, `/v1/schedule*`, `/v1/events*`, `GET /v1/me/schedule` (`auth::DEVICE_ROUTES`) | the CRM routes the app shows natively: they take the device token in place of the cookie, with the user's CRM rights. Installers and template makers (`PhoneUser.portal` `installer` / `template`, the `/v1/app/shell` rule) get only their own schedule in the app and never register for calls |
| `/v1/phone/web/*`, `PUT /v1/phone/calls/recording` | the CRM's phone dock (cookie): the browser registers as a phone of its own (`platform = 'web'`, `device_sessions.web_session_id` = the cookie's session), gets login JWTs (the app's limits), says while it is connected (`presence`: only then it rings), declines; the Record button of a live call (also the app's). Call via my cell and cell verification take the cookie too |
| `/v1/phone/admin/numbers*`, `/v1/phone/admin/devices*` | company admins (cookie): numbers and who they ring, importing them from Telnyx, signing phones out |
| `/v1/phone/admin/ports*` | company admins (cookie): porting existing numbers to Telnyx (portability, draft orders, business details, LOA and bill, port date, submit, cancel, comments) |
| `POST /v1/webhooks/telnyx/{token}/porting` | Telnyx's port-in notifications (Ed25519, required; only a nudge to read the order again) |
| `/v1/phone/admin/tendlc*` | the company's Managers (cookie; the Manager position, not every admin): the 10DLC texting registration, saved as drafts and marked ready |
| `/v1/superuser/tendlc`, `/v1/superuser/companies/{id}/tendlc*` | superusers (cookie): review a registration, its price, send it to Telnyx, read it again, send it again after a rejection, assign the company's numbers |
| `POST /v1/webhooks/telnyx/{token}/10dlc` | Telnyx's 10DLC brand, campaign and number notifications (Ed25519, required) |
| `GET`/`PUT /v1/superuser/companies/{id}/telnyx` | superusers (cookie): a company's Telnyx setup and its `webhookBase` |
| `POST /v1/webhooks/telnyx/{token}/...` | Telnyx (`token` names the company): TeXML forms, which must carry the company's `AccountSid` once its Telnyx account id is saved; Call Control JSON, Ed25519-signed |
| `POST /v1/jobs/phone` | `time-triggered` (job key): sign-outs (lost access, expired or idle sign-ins), leftover credentials, old callbacks, calls left open, the customer phone index, AI notes, call points, Telnyx recording deletes, recording retries, port orders, 10DLC registrations and number assignments |

In the CRM (session cookie) the call log is provider `telnyx` of the call
routes (`routes/calls/telnyx.rs`): `GET /v1/calls/telnyx` lists a deal's or
customer's calls in the `CloudTalk` shape (ids `tx_<call id>`), the
recording route answers 5-minute links to our bucket, and
`POST /v1/call-transcripts` the transcript kept on the call. The bell lists
unseen missed calls, voicemails and AI messages (dismissing one marks it
seen, as in the app) and 911 alerts; users-activity and the call coach
include the calls. A call's end moves the customer's deal on and stops
their flows like a `CloudTalk` call (`phone/outcomes.rs`; 31 to 89 seconds
on a rep's call: once the AI note's transcript shows a conversation).

A user's phones are signed out when they sign out or revoke one, when their
password is reset or changed, and when an admin deletes them or takes their
employee and admin rights away; a company's phones when a superuser deletes
the company; the job signs out anyone who lost access another way, and
sign-ins that expired or went unused for 30 days. An app call always names
its phone with `X-Granite-Device-Id` and proves it with `X-Granite-Call-Auth`,
an HMAC keyed with the phone's call key (see the contract); a SIP user in
`from` must be that phone's. The call key (`callKey`, shown once at sign-in)
is HMAC-SHA256 of `SESSION_SECRET` over the device session id
(`auth/device.rs`): derived again for each call and never stored, so a copy
of the database cannot sign calls. A new `SESSION_SECRET` changes every key:
phones then sign in again before they can call. Emergency codes (911, 112,
933, 988) are never refused; the other N11 codes are.

Calls are matched to customers by `customer_phone_index`, the last 10
digits of each live customer's phone and phone 2 (`phone/customer_index.rs`).
The job fills each company's index once (at most 5,000 customers a run;
until it is done, lookups read the customers' phones as before), then
rewrites the rows of the customers changed (`customers.updated_at`) or added
(new ids) since its last run; the app's own new contacts and added numbers
are indexed at once. A CRM edit reaches the index within a run or two, and
a lookup passes over a row the customer no longer matches, so a stale row
never names the wrong customer. AI notes are tidied by `call_notes.rs`, a
port of the CRM's `sanitizeCallNoteContent` (`app/lib/callAiHelpers.ts`):
change both together.

Porting (`phone/porting.rs`, `telnyx/porting.rs`, `routes/phone/porting.rs`,
`routes/telnyx_webhooks/porting.rs`): an admin moves the company's existing
numbers to Telnyx from Admin → Phone → Port numbers. `phone_port_orders` is our
copy of each Telnyx order (Telnyx may split a request by carrier, number type
or FastPort; the split orders share `request_id`), `phone_port_events` its
timeline and the notifications taken; business details and documents stay in
Telnyx. Every change we send an order carries `webhook_url =
{webhookBase}/porting` and, for when it ports, the company's TeXML app (and
billing group, and messaging profile once texting has one) as its
`phone_number_configuration`, so no portal notification setup is needed.
Orders are read again when opened, on each notification and by the phone job.
Once ported, the numbers are added as unassigned direct lines with the
order's label and checked onto the TeXML app on Telnyx (the job retries until
Telnyx lists them); the admins get a bell and an email. Migration:
`20261023040000_create-phone-port-orders.sql`.

Texting registration, 10DLC (`phone/tendlc.rs`, `phone/tendlc_registry.rs`,
`telnyx/tendlc.rs`, `routes/phone/tendlc.rs`,
`routes/telnyx_webhooks/tendlc.rs`): US carriers block business texts from
local numbers until the company's brand (the business) and campaign (how it
texts) are registered with The Campaign Registry. The company's Managers fill
the form in from Admin → Phone (checked as Telnyx and the carriers check it:
EIN, address, sample texts that name the business and say "Reply STOP to opt
out", ...) and mark it ready; the superusers get a bell. Only a superuser
sends it, from Superuser → Companies → Texting registrations, after seeing
Telnyx's campaign price: Telnyx charges for the brand, three months of the
campaign up front and each number. The brand goes first (never a second one:
a create whose answer never came is looked up first), then the campaign once
TCR registered the brand (its `referenceId` finds a campaign an earlier try
made). Both report to `{webhookBase}/10dlc`, so the company needs its Telnyx
setup first; notifications, a refresh and the phone job move the registration
on, and the company's admins and Managers get a bell when it is approved,
rejected or suspended. After a rejection the form is fixed and sent again: the
brand updated; on a registered campaign only the samples, message flow and
help message change, plus an appeal for a fix outside the form; a new campaign
when TCR refused the first. Once approved, the superuser assigns the company's
active numbers to the campaign (they must be on its messaging profile).
Telnyx's reasons are kept with phone numbers masked. Sole proprietors (no EIN)
are not offered: TCR verifies them with a code sent to the owner's mobile.
Migration: `20261023060000_create-tendlc-registrations.sql`.

Environment (set by hand on `granite-api(-dev)` and `granite-api-stream(-dev)`):

- `TELNYX_API_KEY`: the Telnyx API key; unset, calling answers 503.
- `TELNYX_PUBLIC_KEY`: Telnyx's Ed25519 public key (base64; portal → Keys &
  Credentials → Public Key). Required: Call Control events must be signed,
  and without it they are refused (503, so every app call fails). TeXML
  signatures are checked when Telnyx sends them.
- `PHONE_ALLOW_UNSIGNED_WEBHOOKS`: `1` accepts unsigned Call Control events
  while `TELNYX_PUBLIC_KEY` is unset. Local and tests only; never set it on
  Lambda.
- `TELNYX_FALLBACK_TEXML_URL`: where callers go when a TeXML callback fails
  and the company has no fallback of its own.
- `PHONE_PUBLIC_API_URL`: the root Telnyx calls back (default
  `https://api.granite-manager.com`; `https://api-dev.granite-manager.com` on
  dev).
- `PHONE_RECORDINGS_BUCKET`: the private recordings bucket (below).
- `FCM_SERVICE_ACCOUNT_JSON` (the whole JSON), or `FCM_SERVICE_ACCOUNT_S3_KEY`
  (that JSON's key in `PHONE_RECORDINGS_BUCKET`; Lambda environments max out
  at 4 KB).
- `PHONE_MIN_APP_VERSION`: the oldest app version that may call, for companies
  that set none.
- Tests only: `TELNYX_API_BASE` and `FCM_BASE_URL` point at stand-ins.
- Used too: `ASSEMBLYAI_API_KEY` and `OPEN_AI_SECRET_KEY` (AI notes),
  `LAMBDA_KEY` (the job), `STORAGE_*` (the bucket's credentials).

Telnyx portal, per company. First save the company's Telnyx setup on the
superuser page: the first save makes its `webhookBase`
(`https://api.granite-manager.com/v1/webhooks/telnyx/<token>`).

1. TeXML application: voice URL `{webhookBase}/inbound`, method POST; status
   callback `{webhookBase}/call-status`, method POST; failover URL a TeXML Bin
   that connects the AI Assistant and then takes a voicemail (save the same
   Bin as the setup's fallback TeXML URL). Assign the company's numbers to it,
   then "Import numbers from Telnyx" on the admin Phone page.
2. Credential connection (the phones): webhook URL `{webhookBase}/outbound`,
   webhook API version 2, call parking on (outgoing calls wait for our
   approval), webhook timeout 10 s, SIP URI calling "internal" (our TeXML
   rings the phones at `sip:gencred...@sip.telnyx.com`), the company's
   outbound voice profile (US and Canada), and an Android push credential made
   from the Firebase service account, so Telnyx can wake the app for calls.
   The CRM's browser phone uses the same connection over WebRTC (Telnyx's JS
   SDK with our login JWTs): keep WebRTC allowed on it (the default for
   credential connections) and the outbound voice profile's US/Canada
   destinations. A browser phone is signed out with its CRM session; its
   calls are approved like the app's (`X-Granite-Call-Auth` with its call
   key). On-demand recording of TeXML calls (incoming, Call via my cell)
   needs the Telnyx account id saved in the setup (step 3).
3. Put the TeXML application, credential connection, outbound voice profile,
   billing group and AI Assistant ids in the setup, and the Telnyx account id
   (the `AccountSid` of Telnyx's TeXML requests; once it is saved, TeXML
   requests without it are refused), and switch calling on.
4. Texting (the `telnyx` SMS provider, `phone/texting.rs`): a messaging
   profile with webhook URL `{webhookBase}/messaging` (API v2), the
   company's numbers assigned to it, and the brand / campaign (10DLC) the
   numbers text under approved; then its id in the setup ("Messaging
   profile ID"). Users text from their line and read the threads of every
   number they may call from; inbound files are copied to the bucket, STOP
   opts the phone out (`sms_opt_outs`; automated texts skip it).

Firebase: the app's Firebase project → Project settings → Service accounts →
Generate new private key. That JSON is both our FCM sender (upload it to the
recordings bucket and set `FCM_SERVICE_ACCOUNT_S3_KEY`) and Telnyx's Android
push credential.

Recordings bucket: an S3 bucket that blocks all public access, with default
encryption, that the storage IAM user may read and write (`s3:GetObject`,
`s3:PutObject`). Recordings are copied there from Telnyx
(`phone/{company}/{yyyy}/{mm}/{call id}-{call|voicemail}.mp3`, Telnyx's copy
deleted once ours is checked) and played through 5-minute presigned links.
The CRM's call player fetches the audio, so the bucket's CORS must allow
`GET` and `HEAD` from `https://granite-manager.com` and
`https://*.granite-manager.com`.

Migrations: apply `20261017100000_create-phone-tables.sql`,
`20261017100100_alter-customers-add-updated-at.sql`,
`20261017100200_alter-phone-call-points-add-telnyx.sql`,
`20261017100300_create-customer-phone-index.sql`,
`20261023010000_create-telnyx-texting.sql`,
`20261023010100_alter-telnyx-sms-add-line-index.sql`,
`20261023030000_alter-phone-devices-add-web.sql` (browser phones, the Record
button), `20261023040000_create-phone-port-orders.sql` and
`20261023060000_create-tendlc-registrations.sql` to RDS (`duplicate`,
then `main`) before deploying the code, by hand as usual, from an LF checkout
(sqlx checksums the files' bytes):
`cargo sqlx migrate run --source migrations --database-url '<RDS URL>'`. Then
deploy the API, then `time-triggered` (it calls the job).

Tests: `api/tests/phone_*.rs`, one area per file, with shared fixtures and one
stand-in for Telnyx, FCM, AssemblyAI and OpenAI in `tests/phone_common/`. With
the local MySQL up (`make api-db-up`), run a file at a time, e.g.
`cargo test -p api --test phone_jobs`. `.github/workflows/api-pr.yml` runs
them all and the OpenAPI checks on every pull request to main.

## Dev environment

`granite-api-dev`, `granite-api-stream-dev` and `granite-webhooks-dev` never touch
production:

- `DATABASE_URL` points at the `duplicate` database, never `main`.
- `granite-api-stream-dev` is `granite-api-dev`'s environment plus
  `LAMBDA_RESPONSE_STREAMING=1` behind a `RESPONSE_STREAM` function URL (15 min,
  1 GB). The dev site has no stream host of its own: `POST /v1/ai/chat` and
  `POST /v1/ai/design` on `api-dev.granite-manager.com` are routed to it by the
  CloudFront distribution (60 s between bytes), so the session cookie reaches it.
  Without that, a long AI answer on dev hits `granite-api-dev`'s 30 s limit.
- Their secrets (`SESSION_SECRET`, `LAMBDA_KEY`, `WEBHOOKS_KEY`,
  `WEBHOOK_SECRET`) are their own: dev's `WEBHOOKS_KEY` is a token in the
  dev database.
- `SESSION_COOKIE_DOMAIN=none`: the dev session cookie is host-only on
  `api-dev.granite-manager.com`, so signing in on dev never replaces the
  production cookie on `.granite-manager.com`. The browser then sends both
  `__session` cookies to dev; the API uses the one that verifies.
- No `AES_KEY`: the integration tokens copied into `duplicate` (QuickBooks,
  RingCentral) stay unreadable, so dev cannot refresh production's tokens.
  CloudTalk, RingCentral, QuickBooks, Calendly and Telegram are not configured.
- Files go to `granite-storage-dev-<account>` (us-east-1) through the
  `granite-storage-dev` IAM user, which can reach only that bucket.

`infra/dev-api.yaml` (stack `granite-api-dev`, us-east-1) holds the dev bucket,
its IAM user, and CloudFront + certificate + DNS for
`https://api-dev.granite-manager.com` in front of the function URL:

```
aws cloudformation deploy --region us-east-1 --stack-name granite-api-dev \
  --template-file infra/dev-api.yaml --capabilities CAPABILITY_NAMED_IAM \
  --parameter-overrides FunctionUrlDomain=<granite-api-dev function URL host> \
  StreamFunctionUrlDomain=<granite-api-stream-dev function URL host>
```

The frontend's `make deploy-dev` builds against `api-dev.granite-manager.com`.

Call settings (the app's Settings: incoming call rules, forward all calls,
incoming call information, hold music; the company's spam and blocked
numbers for admins) are `phone/call_rules.rs`, `phone/blocking.rs` and
`phone/hold_music.rs`; `phone::routing::route` applies them to every
incoming call before anything rings. Semantics, and what Telnyx's docs
confirm versus what needs a real call: `docs/call-settings.md` in
granite-phone. The built-in hold music (`data/hold-music/`, CC0) is served
at `GET /v1/public/hold-music/{track}.mp3`.
