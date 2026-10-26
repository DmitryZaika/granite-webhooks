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
