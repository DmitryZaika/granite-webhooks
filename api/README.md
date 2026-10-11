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

`me`, `customers`, `users`, `stones`, `sinks` and `faucets` exist so far; create
the others as their first route is migrated, following the same four files.

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
| 7000-7003   | sales                                                       |

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

`make deploy-api` creates/updates the `granite-api` Lambda. It needs these
environment variables: `DATABASE_URL`, `SESSION_SECRET` (same as Remix),
optionally `API_ALLOWED_ORIGINS`. Expose it on `api.granite-manager.com`
(CloudFront or an API Gateway custom domain in front of the function URL) so
the session cookie is sent; then set `VITE_GRANITE_API_URL` in the frontend's
`.env.prod` (pages that use the API do not work without it).
