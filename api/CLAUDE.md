# api crates

Read `api/README.md` before changing routes: it is the playbook for these
crates, and its Layout section says which crate in `api/crates/` holds what.

## Checks and tests while you work

The api is the `api` crate plus the crates in `api/crates/` (`api-crm`,
`api-followups`, ...). Check and test what you changed, not everything:

- `cargo clippy -p api-<crate> --all-targets` for the crate you edited. It
  type-checks too, so do not also run `cargo check`.
- `cargo test -p api --test <area> <file>::` for one integration test file,
  `api/tests/<area>/<file>.rs`. The test databases are kept between runs, so
  a file's tests start at once.
- `cargo test -p api-<crate> <filter>` for unit tests in `api/crates/<crate>/src`.
- After changing what other crates use (a signature, a type, a route's
  request or response): `cargo clippy --workspace --all-targets`.
- Before you push: `cargo test -p 'api*'` (every api test, about 30 s).

The first run of a different selection (`-p api-crm`, `-p api`,
`--workspace`) builds its own copy of some dependencies once; after that each
recompiles only what changed.

## Every endpoint must be documented for AI agents

AI agents use this API only through its OpenAPI spec (`api/openapi.json`,
served at `GET /openapi.json`). An endpoint without good descriptions is
unusable to them. Whenever you add or change an endpoint, a parameter, or a
response field, in the same change:

1. Annotate the handler with `#[crate::openapi::path(...)]` (utoipa's
   `#[utoipa::path]` arguments; utoipa only runs with the `openapi` feature,
   `make api-openapi`): method, full `path`,
   explicit verb_noun `operation_id`, `tag`, `params`/`request_body`, every
   response status with a description (`body = ErrorBody` for errors), and
   `security(("session_cookie" = []))`. Copy the shape from
   `crates/crm/src/routes/customers/handlers.rs`.
2. Write the handler doc comment for an agent that has never seen the code:
   a one-line imperative summary, a blank line, then what it returns, which
   filters and defaults apply, what is hidden, limits, and which route to
   call next.
3. Derive `ToSchema` (bodies) / `IntoParams` (query structs), imported from
   `crate::openapi`, and put a doc
   comment on every type and field: business meaning, format/units, allowed
   values, when it is null. Don't describe the SQL.
4. Register the handler with `.routes(routes!(handlers::name))`, never
   `.route(...)`.
5. Run `make api-openapi` to regenerate `api/openapi.json`, then
   `cargo test -p api --features openapi --test app openapi::` (no database
   needed). Commit the regenerated `openapi.json` with the change.

If a doc comment you are editing is now wrong because the behavior changed,
fix it; stale descriptions mislead agents worse than missing ones.
