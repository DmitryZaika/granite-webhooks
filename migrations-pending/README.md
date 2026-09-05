# Pending migrations

Migrations written ahead of time that must NOT be applied yet. sqlx only reads
`migrations/`; move a file there (keeping its timestamp) when its precondition holds.

| File | Apply when |
|---|---|
| `20260904120200_drop-users-onboarding-columns.sql` | Every precondition below holds. |

## `20260904120200_drop-users-onboarding-columns.sql`

Drops `users.onboarding` and `users.onboarding_progress`. Apply only when all of
the following are true:

1. The SPA release that reads the `user_onboarding*` tables is deployed.
2. The backfill has been run **twice**: once after migration A
   (`20260904120000_create-user-onboarding-tables.sql`) and once more right
   after that SPA deploy. The second run is what captures everything the legacy
   client wrote between the two — it is idempotent and never overwrites live rows.
3. Nothing reads the two columns any more. As of this wave they still have
   readers, and each must be removed or rewritten first:
   - `general_datebase/app/utils/onboardingBackfill.server.ts` — `BackfillSourceRow`
     and the `SELECT id, company_id, onboarding, onboarding_progress FROM users`
     sweep in `runOnboardingBackfill`;
   - `general_datebase/scripts/backfill-onboarding-state.ts` — the entry point
     for that sweep;
   - `parseSavedOnboardingProgress` in
     `general_datebase/app/utils/onboardingSummary.ts` — parses the blob;
   - `tests/testDatabase.ts` — the `onboarding` / `onboarding_progress` columns in
     the harness `users` DDL, plus any test that seeds them.

   Deleting the backfill module is the last step of the cutover, not the first:
   until the columns are gone it is the only way to re-run the migration.
