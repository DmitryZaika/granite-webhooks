-- Errors of the time-triggered scheduler Lambda (distinct_id server-time-triggered).
ALTER TABLE posthog_autofix_runs
  MODIFY COLUMN source ENUM('web', 'granite-api', 'server-webhooks', 'server-time-triggered', 'legacy-node', 'unknown') NULL;
