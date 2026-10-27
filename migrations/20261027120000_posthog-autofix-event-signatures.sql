-- The PostHog auto-fix intake now reads $exception events, not issues: a
-- ledger row is one bug (its signature) per signature window, found by its
-- events whatever issue PostHog put them in. A catch-all issue that holds
-- several bugs may own several rows, so posthog_issue_id is no longer unique;
-- it names the issue of the bug's latest event (moved while the row waits).
--
-- event_uuid is that event, sent to the fix workflow. api_route, api_status
-- and error_source are the event's own properties, read by the noise gates
-- and by the link from a web 5xx to the api error of the same route.
-- first_seen_at / last_seen_at bound the bug's events; issue_created_at is
-- now its first event (the settle clock).
--
-- Rows the issue intake left in `discovered` have no signature, so their bug
-- would get a second row from its events: they are retired as `superseded`.
-- The read cursor is a `posthog_autofix_notices` row (kind `events_read`).

ALTER TABLE posthog_autofix_runs
  DROP INDEX uq_posthog_autofix_issue,
  ADD KEY idx_posthog_autofix_issue (posthog_issue_id),
  ADD COLUMN event_uuid    CHAR(36)          NULL AFTER posthog_issue_id,
  ADD COLUMN api_route     VARCHAR(255)      NULL AFTER lib,
  ADD COLUMN api_status    SMALLINT UNSIGNED NULL AFTER api_route,
  ADD COLUMN error_source  VARCHAR(32)       NULL AFTER api_status,
  ADD COLUMN first_seen_at DATETIME(3)       NULL AFTER issue_created_at,
  ADD COLUMN last_seen_at  DATETIME(3)       NULL AFTER first_seen_at,
  ADD KEY idx_posthog_autofix_signature_discovered (signature, discovered_at),
  ADD KEY idx_posthog_autofix_seen (source, last_seen_at);

UPDATE posthog_autofix_runs
   SET status = 'skipped', skip_reason = 'superseded'
 WHERE status = 'discovered' AND signature IS NULL;
