-- The AI check of a 10DLC texting registration: when a company's Manager
-- marks it ready, an AI reads the form and the company's quote form, privacy
-- policy and texting terms pages the way the carriers do. The Manager gets
-- an email either way: what to change (their text and the text to use), or
-- that it passed; a registration that passed is ready for the superusers
-- (bell and email). Only adds columns.
--
-- ai_check_status: pending (waiting for the job), running, passed, changes
-- (the Manager was sent what to change) or failed (the AI could not run; it
-- went to the superusers unchecked). NULL when never checked.
-- ai_check_report: the result, {"summary": ..., "fixes": [...], "unclear":
-- [...], "pages": [...]} (the API's camelCase fields).

ALTER TABLE tendlc_registrations
  ADD COLUMN ai_check_status               VARCHAR(16)      NULL AFTER refreshed_at,
  ADD COLUMN ai_check_requested_at         DATETIME         NULL AFTER ai_check_status,
  ADD COLUMN ai_check_requested_by_user_id INT              NULL AFTER ai_check_requested_at,
  -- a run in progress; one older than 15 minutes is taken again
  ADD COLUMN ai_check_started_at           DATETIME         NULL AFTER ai_check_requested_by_user_id,
  ADD COLUMN ai_check_attempts             TINYINT UNSIGNED NOT NULL DEFAULT 0 AFTER ai_check_started_at,
  ADD COLUMN ai_checked_at                 DATETIME         NULL AFTER ai_check_attempts,
  ADD COLUMN ai_check_report               JSON             NULL AFTER ai_checked_at,
  ADD KEY idx_tendlc_registrations_ai_check (ai_check_status, ai_check_started_at);
