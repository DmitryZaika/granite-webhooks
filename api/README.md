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
    state.rs             AppState (pool, SESSION_SECRET, CORS origins)
    error.rs             ApiError -> JSON {"error": "..."} with 400/401/403/500
    auth/
      cookie.rs          React Router `__session` cookie decode/encode
      session.rs         session -> user lookup, super-admin company switch
      mod.rs             extractors: CurrentUser, EmployeeUser, AdminUser
    routes/
      mod.rs             merges every domain router
      <domain>/
        mod.rs           router() with full paths + a doc list of the routes
        handlers.rs      auth extractor + params -> query -> Json
        queries.rs       SQL, one function per database read
        schemas.rs       request params, response rows (serde + FromRow)
  tests/
    common/mod.rs        test server, signed cookies, seeded session ids
    <domain>.rs          integration tests per domain
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

Only `me` and `customers` exist so far; create the others as their first route
is migrated, following the same four files.

## Rules for every migrated endpoint

1. **One endpoint per database read.** If a Remix loader runs three queries,
   write three endpoints. The frontend calls them (in parallel where possible)
   and composes the page. Don't add "page" endpoints that bundle reads.
2. **Same SQL as Remix.** Copy the query, keep column aliases so the JSON
   field names match the TypeScript type the page already uses. Use runtime
   `sqlx::query_as` / `QueryBuilder` with `#[derive(FromRow)]` (no
   compile-time macros), so building this crate never needs a database.
3. **Always scope by `user.company_id`** from the auth extractor, including
   lookups by ids the client sends (Remix trusted ids it had just queried
   itself; the API cannot).
4. **Keep the composition in the frontend.** The pure "rows -> page data"
   code moves from the Remix loader into a client-safe module that the new
   client loader calls.
5. **Match wire types.** Remix sent mysql2 values: `Date` for timestamps,
   strings for `DECIMAL`, numbers for `COUNT`. The API sends ISO strings
   (`serde_helpers::js_date`), `CAST(... AS CHAR)` for decimals, `i64` for counts;
   the client loader converts ISO strings back to `Date`.
6. **Errors become status codes.** Remix's `selectMany` swallowed SQL errors
   and returned `[]`; the API returns 500. Invalid params return 400.
7. **Tests are the proof.** The old loader is deleted in the same change, so
   `tests/<domain>.rs` must cover every branch of the old SQL against seeded
   data: exact JSON for at least one row per response shape, each filter,
   NULL/deleted rows, tenant isolation, and the auth cases.

### Steps to migrate a route

1. Read the Remix loader; list each database read and the permission check.
2. Add rows to `seed/local_seed.sql` (append only; ids >= 100) covering each
   branch of the query (filters, NULLs, deleted rows, a second company).
3. For each read: add a function in `queries.rs`, a handler, a route in the
   domain `mod.rs`, and tests. `make api-test`.
4. Frontend: add typed callers in `app/lib/api/<domain>.ts`, move the
   composition into a client-safe module, replace the route's `loader` with a
   `clientLoader`, and delete the old loader code. See
   `docs/backend-migration.md` in that repo.

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
