-- The PostHog auto-fix ledger: one row per PostHog error tracking issue the
-- api job `POST /v1/jobs/posthog-autofix` has seen since the cutover
-- (POSTHOG_AUTOFIX_START_AT). A row records whether the issue was fixed by
-- the GitHub workflow `posthog-autofix.yml`, skipped, or folded into another
-- run as a duplicate, and drives that run through its states:
--
--   discovered -> queued | skipped | duplicate
--   queued     -> dispatched | previewed (dry run) | failed (dispatch kept failing)
--   dispatched -> running | queued (dispatch retry) | failed | timed_out
--   running    -> succeeded | failed | timed_out | queued (usage limit, retried later)
--   timed_out  -> succeeded | failed (a late result, once)
--
-- A system table, not tenant data: errors belong to the product, not to a
-- company, so it has no company_id. Only the api crate reads or writes it.
--
-- posthog_issue_id is UNIQUE so a PostHog issue can start at most one run
-- (a reopened issue keeps its id and never starts a second one). signature
-- groups issues that are the same bug (see api/src/posthog_autofix/signature.rs).

CREATE TABLE IF NOT EXISTS posthog_autofix_runs (
  id                     BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
  posthog_issue_id       CHAR(36)        NOT NULL,
  issue_created_at       DATETIME(3)     NOT NULL,
  issue_name             VARCHAR(255)    NOT NULL,
  issue_description      VARCHAR(2000)   NOT NULL,
  -- Filled once the issue has settled and its events were read.
  source                 ENUM('web', 'granite-api', 'server-webhooks', 'legacy-node', 'unknown') NULL,
  lib                    VARCHAR(64)     NULL,
  occurrences            INT UNSIGNED    NULL,
  users                  INT UNSIGNED    NULL,
  signature              CHAR(64)        NULL,
  duplicate_of_run_id    BIGINT UNSIGNED NULL,
  status                 ENUM('discovered', 'queued', 'dispatched', 'running', 'succeeded',
                              'failed', 'timed_out', 'skipped', 'duplicate', 'previewed')
                                         NOT NULL DEFAULT 'discovered',
  skip_reason            VARCHAR(32)     NULL,
  -- What the agent concluded; set by the workflow's result callback.
  outcome                ENUM('fixed_with_test', 'fixed_without_test', 'not_a_code_bug',
                              'not_reproducible', 'duplicate', 'needs_human') NULL,
  verification           ENUM('verified', 'unverified', 'not_applicable') NULL,
  target_repo            ENUM('granite-webhooks', 'general_datebase') NULL,
  pr_url                 VARCHAR(512)    NULL,
  github_run_url         VARCHAR(512)    NULL,
  cost_usd               DECIMAL(8, 2)   NULL,
  attempts               TINYINT UNSIGNED NOT NULL DEFAULT 0,
  -- A queued row is not dispatched before this (dispatch backoff, usage limit).
  retry_at               DATETIME(3)     NULL,
  last_error             VARCHAR(512)    NULL,
  -- The issue's PostHog status when last read: resolved -> active after a fix
  -- is a regression, emailed once (regression_notified_at).
  last_issue_status      VARCHAR(16)     NULL,
  -- The result callback's body, for the email and for audits.
  result                 JSON            NULL,
  discovered_at          DATETIME(3)     NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
  enriched_at            DATETIME(3)     NULL,
  dispatched_at          DATETIME(3)     NULL,
  started_at             DATETIME(3)     NULL,
  finished_at            DATETIME(3)     NULL,
  notified_at            DATETIME(3)     NULL,
  regression_notified_at DATETIME(3)     NULL,
  -- The PostHog issue was assigned to the fix's reviewer (write-back).
  posthog_assigned_at    DATETIME(3)     NULL,
  updated_at             DATETIME(3)     NOT NULL DEFAULT CURRENT_TIMESTAMP(3)
                                         ON UPDATE CURRENT_TIMESTAMP(3),
  UNIQUE KEY uq_posthog_autofix_issue (posthog_issue_id),
  KEY idx_posthog_autofix_status (status, retry_at),
  KEY idx_posthog_autofix_signature (signature, status),
  KEY idx_posthog_autofix_issue_created (issue_created_at),
  CONSTRAINT fk_posthog_autofix_duplicate_of
    FOREIGN KEY (duplicate_of_run_id) REFERENCES posthog_autofix_runs (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- Claims on once-per-window work: the PostHog poll (one per poll window) and
-- the storm digest email (one per window). The INSERT that creates the row
-- wins; a concurrent tick's INSERT IGNORE changes nothing.
CREATE TABLE IF NOT EXISTS posthog_autofix_notices (
  kind         VARCHAR(32) NOT NULL,
  window_start DATETIME    NOT NULL,
  created_at   DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
  PRIMARY KEY (kind, window_start)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
