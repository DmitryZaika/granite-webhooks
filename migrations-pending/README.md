# Pending migrations

Migrations written ahead of time that must NOT be applied yet. sqlx only reads
`migrations/`; move a file there (keeping its timestamp) when its precondition
holds, and delete its section below.

| File | Apply when |
|---|---|
| _(none pending)_ | |

## History

`20260904120200_drop-users-onboarding-columns.sql` moved to `migrations/` on
2026-09-11. Its preconditions no longer needed a human to sequence them:

- The SPA release that reads the `user_onboarding*` tables is on
  `feat/new-feature-onboarding`, and every reader of the two legacy columns has
  been deleted (`onboardingBackfill.server.ts`, `scripts/backfill-onboarding-state.ts`,
  `parseSavedOnboardingProgress`, and the harness `users` DDL in
  `tests/testDatabase.ts`).
- `users.onboarding` turned out to be a `TIMESTAMP` — "finished the first-run
  journey" — not a blob. Carrying it into `user_onboarding.completed_at` is two
  idempotent statements, so they were folded into the migration itself rather
  than left as a script that had to be run first. The migration is now
  self-sufficient and order-independent.
- Rich `users.onboarding_progress` blobs (tasks, modules, ledger) were the only
  thing the TypeScript backfill handled that SQL cannot. On the shared RDS
  exactly one user still carries such a blob, and that user already has a live
  journey row with `reset_at` set, which the backfill would have skipped anyway.
