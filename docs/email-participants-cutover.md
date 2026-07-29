# Email participants cutover

Migration: `migrations/20260717234741_separate-out-email.sql`

Creates `email_participants`, backfills from `emails.sender_*` / `emails.receiver_*`, then drops those columns.

## Order (required — do not skip)

1. Deploy **granite-webhooks** (SES ingest writes participants; does not select dropped columns).
2. Deploy **general_datebase** (readers + send API use participants).
3. **Immediately** run the migration on that environment.

Do not leave old code on a migrated DB, or new code long on a pre-migration DB.

## Smoke checks after migrate

- Inbound multi-Cc eml → `email_participants` rows for from/to/cc (bcc only if present in MIME).
- Unread / notifications show for To **and** Cc recipients.
- EmailChat shows From/To/Cc for multi-recipient messages.
- Reply = From only (excludes self); Reply all = From+To in To, prior Cc in Cc, never Bcc.
- Outbound send writes from/to/cc/bcc participant rows; reply keeps same `thread_id`.
- Inbound From that matches a non-employee `users` row must **not** get `user_id` (stays inbound).

## Known limits

- Inbound Bcc is often absent from MIME for non-Bcc recipients.
- Cutover has no dual-write / graceful degrade — migrate in the same window as deploys.
