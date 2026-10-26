# webhooks crates

Read `webhooks/README.md` before changing this Lambda: its Layout section
says which crate in `webhooks/crates/` holds what.

## Checks and tests while you work

The Lambda is the `webhooks` crate plus the crates in `webhooks/crates/`
(`webhooks-core`, `webhooks-email`, `webhooks-phone`, `webhooks-leads`).
Check and test what you changed, not everything:

- `cargo clippy -p webhooks-<crate> --all-targets` for the crate you edited.
  It type-checks too, so do not also run `cargo check`.
- `cargo test -p webhooks-<crate> <module>::` for one module's tests.
- After changing what other crates use (anything in `webhooks-core`):
  `cargo clippy -p 'webhooks*' --all-targets`.
- Before you push: `cargo test -p 'webhooks*'` (every webhooks test).

Test helpers are in `webhooks/crates/core/src/tests/`; tests reach them as
`crate::tests::...` in every crate, and `crate::tests::utils::new_test_app`
serves the crate's own routes.
