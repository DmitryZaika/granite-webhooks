-- Onboarding state, normalised. Replaces users.onboarding (first-run flag) and
-- users.onboarding_progress (JSON document). See
-- general_datebase/docs/superpowers/specs/2026-09-04-onboarding-server-state-design.md

-- 1. The first-run journey, one row per user. completed_at NULL = first run not
--    finished (the meaning users.onboarding IS NULL had).
CREATE TABLE user_onboarding (
    user_id               INT          NOT NULL,
    company_id            INT          NOT NULL,
    current_module_id     VARCHAR(64)  NULL,
    current_step_key      VARCHAR(128) NULL,
    dismissed_at          DATETIME     NULL,
    completed_at          DATETIME     NULL,
    feedback_dismissed_at DATETIME     NULL,
    reset_at              DATETIME     NULL,
    updated_at            DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP
                                       ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (user_id),
    KEY idx_user_onboarding_company (company_id),
    CONSTRAINT fk_user_onboarding_user
        FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    CONSTRAINT fk_user_onboarding_company
        FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- 2. Journey modules the user has finished, or is resuming inside (a set).
CREATE TABLE user_onboarding_modules (
    user_id      INT          NOT NULL,
    company_id   INT          NOT NULL,
    module_id    VARCHAR(64)  NOT NULL,
    step_key     VARCHAR(128) NULL,
    completed_at DATETIME     NULL,
    updated_at   DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP
                              ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (user_id, module_id),
    KEY idx_user_onboarding_modules_company (company_id, module_id),
    CONSTRAINT fk_user_onboarding_modules_user
        FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    CONSTRAINT fk_user_onboarding_modules_company
        FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- 3. Feature tasks, one row per user x task.
CREATE TABLE user_feature_tasks (
    user_id        INT          NOT NULL,
    company_id     INT          NOT NULL,
    task_id        VARCHAR(64)  NOT NULL,
    version        INT          NOT NULL,
    status         VARCHAR(16)  NOT NULL,   -- in_progress | completed | closed
    step_key       VARCHAR(128) NULL,
    done_steps     JSON         NOT NULL,   -- { stepKey: 'action' | 'manual' }
    started_at     DATETIME     NOT NULL,
    completed_at   DATETIME     NULL,
    closed_at      DATETIME     NULL,
    last_active_at DATETIME     NOT NULL,
    updated_at     DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP
                                ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (user_id, task_id),
    KEY idx_user_feature_tasks_company_task (company_id, task_id, status),
    CONSTRAINT fk_user_feature_tasks_user
        FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    CONSTRAINT fk_user_feature_tasks_company
        FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- 4. Points, append-only. A total is SUM(points). Nothing is edited in place.
CREATE TABLE user_points_ledger (
    id         BIGINT      NOT NULL AUTO_INCREMENT,
    user_id    INT         NOT NULL,
    company_id INT         NOT NULL,
    task_id    VARCHAR(64) NOT NULL,
    points     INT         NOT NULL,        -- negative for a reversal
    reason     VARCHAR(32) NOT NULL,        -- completed | repriced | retired | reset | backfill
    awarded_at DATETIME    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    KEY idx_points_user_task (user_id, task_id),
    KEY idx_points_company (company_id),
    CONSTRAINT fk_user_points_ledger_user
        FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    CONSTRAINT fk_user_points_ledger_company
        FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
