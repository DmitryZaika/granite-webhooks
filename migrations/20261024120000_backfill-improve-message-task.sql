-- Completes the `improve-message` feature task for users who already improve
-- drafts.
--
-- Since app commit 2ae4c584 (2026-10-12) every successful Improve completes the
-- task, tour or not. Users who only improved drafts before that never got it,
-- so the app still offers them a tour of a feature they already use.
-- `ai_usage_log` (feature `email_improve`, Email and SMS alike) holds every
-- past use: complete the task from it and pay its 5 points once, as a
-- `backfill` ledger row.
--
-- Uses older than the user's `feature_tasks_reset_at` do not count: a reset
-- user is meant to look new. A completed row is never touched, so running this
-- again changes nothing.

-- 1. Points first, while "not completed" still names exactly the users steps
--    2 and 3 complete. Nothing when the task has already paid something.
INSERT INTO user_points_ledger (user_id, company_id, task_id, points, reason)
SELECT u.id, u.company_id, 'improve-message', 5, 'backfill'
  FROM users u
  JOIN (SELECT DISTINCT l.user_id
          FROM ai_usage_log l
          LEFT JOIN user_onboarding o ON o.user_id = l.user_id
         WHERE l.feature = 'email_improve'
           AND l.success <> 0
           AND (o.feature_tasks_reset_at IS NULL OR l.created_at > o.feature_tasks_reset_at)
       ) used ON used.user_id = u.id
  LEFT JOIN user_feature_tasks t ON t.user_id = u.id AND t.task_id = 'improve-message'
 WHERE u.is_deleted = 0
   AND u.company_id IS NOT NULL
   AND (t.user_id IS NULL OR (t.company_id = u.company_id AND t.status <> 'completed'))
   AND COALESCE((SELECT SUM(p.points) FROM user_points_ledger p
                  WHERE p.user_id = u.id AND p.company_id = u.company_id
                    AND p.task_id = 'improve-message'), 0) = 0;

-- 2. Users with no task row. LEFT JOIN, not ON DUPLICATE KEY: a live row is
--    never touched here.
INSERT INTO user_feature_tasks
    (user_id, company_id, task_id, version, status, step_key, done_steps,
     started_at, completed_at, last_active_at)
SELECT u.id, u.company_id, 'improve-message', 1, 'completed', NULL,
       JSON_OBJECT('compose-send', 'action'), used.first_at, used.last_at, used.last_at
  FROM users u
  JOIN (SELECT l.user_id, MIN(l.created_at) AS first_at, MAX(l.created_at) AS last_at
          FROM ai_usage_log l
          LEFT JOIN user_onboarding o ON o.user_id = l.user_id
         WHERE l.feature = 'email_improve'
           AND l.success <> 0
           AND (o.feature_tasks_reset_at IS NULL OR l.created_at > o.feature_tasks_reset_at)
         GROUP BY l.user_id
       ) used ON used.user_id = u.id
  LEFT JOIN user_feature_tasks t ON t.user_id = u.id AND t.task_id = 'improve-message'
 WHERE u.is_deleted = 0
   AND u.company_id IS NOT NULL
   AND t.user_id IS NULL;

-- 3. Rows still in progress or closed (a tour left unfinished, then Improve
--    used anyway).
UPDATE user_feature_tasks t
  JOIN users u ON u.id = t.user_id AND u.company_id = t.company_id
  JOIN (SELECT l.user_id, MAX(l.created_at) AS last_at
          FROM ai_usage_log l
          LEFT JOIN user_onboarding o ON o.user_id = l.user_id
         WHERE l.feature = 'email_improve'
           AND l.success <> 0
           AND (o.feature_tasks_reset_at IS NULL OR l.created_at > o.feature_tasks_reset_at)
         GROUP BY l.user_id
       ) used ON used.user_id = t.user_id
   SET t.done_steps = JSON_SET(t.done_steps, '$."compose-send"', 'action'),
       t.step_key = NULL,
       t.completed_at = used.last_at,
       t.last_active_at = GREATEST(t.last_active_at, used.last_at),
       t.status = 'completed'
 WHERE t.task_id = 'improve-message'
   AND t.status <> 'completed'
   AND u.is_deleted = 0;
