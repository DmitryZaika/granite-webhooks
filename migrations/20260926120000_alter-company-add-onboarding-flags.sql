-- Per-company rollout of the new onboarding tours and the points leaderboard.
-- See general_datebase/docs/superpowers/specs/2026-09-26-onboarding-rollout-and-ux-design.md §2.
--
-- NULL means "follow the all-companies switch" in global_feature_flags; 1 or 0
-- is an explicit per-company choice that wins over it. Companies created later
-- get NULL, so they follow the global switch with no code at creation.
ALTER TABLE company
  ADD COLUMN onboarding_enabled TINYINT(1) NULL DEFAULT NULL,
  ADD COLUMN leaderboard_enabled TINYINT(1) NULL DEFAULT NULL;

-- One row per rollout feature. A missing row reads as off.
CREATE TABLE global_feature_flags (
    feature_key        VARCHAR(64) NOT NULL,
    enabled_for_all    TINYINT(1)  NOT NULL DEFAULT 0,
    updated_at         DATETIME    NOT NULL DEFAULT CURRENT_TIMESTAMP
                                   ON UPDATE CURRENT_TIMESTAMP,
    updated_by_user_id INT         NULL,
    PRIMARY KEY (feature_key)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

INSERT INTO global_feature_flags (feature_key, enabled_for_all)
VALUES ('onboarding', 0), ('leaderboard', 0);

-- The pilot (Granite Depot of Indianapolis) is explicitly on from the start.
-- Where no company 1 exists this changes nothing.
UPDATE company SET onboarding_enabled = 1, leaderboard_enabled = 1 WHERE id = 1;

-- The Hub's Restart now resets the first-run journey only, so reset_at fences
-- journey events only. The tester reset of feature tours gets its own fence.
-- Past resets fenced both kinds of event, so they keep doing so.
ALTER TABLE user_onboarding
  ADD COLUMN feature_tasks_reset_at DATETIME(3) NULL AFTER reset_at;

UPDATE user_onboarding SET feature_tasks_reset_at = reset_at WHERE reset_at IS NOT NULL;
