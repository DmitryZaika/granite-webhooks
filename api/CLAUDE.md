# api crate

Read `api/README.md` before changing routes: it is the playbook for this crate.

## Every endpoint must be documented for AI agents

AI agents use this API only through its OpenAPI spec (`api/openapi.json`,
served at `GET /openapi.json`). An endpoint without good descriptions is
unusable to them. Whenever you add or change an endpoint, a parameter, or a
response field, in the same change:

1. Annotate the handler with `#[utoipa::path(...)]`: method, full `path`,
   explicit verb_noun `operation_id`, `tag`, `params`/`request_body`, every
   response status with a description (`body = ErrorBody` for errors), and
   `security(("session_cookie" = []))`. Copy the shape from
   `src/routes/customers/handlers.rs`.
2. Write the handler doc comment for an agent that has never seen the code:
   a one-line imperative summary, a blank line, then what it returns, which
   filters and defaults apply, what is hidden, limits, and which route to
   call next.
3. Derive `ToSchema` (bodies) / `IntoParams` (query structs) and put a doc
   comment on every type and field: business meaning, format/units, allowed
   values, when it is null. Don't describe the SQL.
4. Register the handler with `.routes(routes!(handlers::name))`, never
   `.route(...)`.
5. Run `make api-openapi` to regenerate `api/openapi.json`, then
   `cargo test -p api --test openapi` (no database needed). Commit the
   regenerated `openapi.json` with the change.

If a doc comment you are editing is now wrong because the behavior changed,
fix it; stale descriptions mislead agents worse than missing ones.
