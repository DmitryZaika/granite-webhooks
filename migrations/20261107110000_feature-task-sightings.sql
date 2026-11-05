-- Tour "sightings": the first time a user met a guided tour's opening control
-- on the tour's own page (on screen) before the tour ever ran. The app starts
-- a tour by itself only on a later local day than its sighting (the "second
-- visit" rule), so the sighting must follow the user across devices.
--
-- A sighting is not tour progress: it never creates a user_feature_tasks row
-- (any row there counts as "met" for deferral and announcements). The first
-- sighting is kept; one from before the user's feature-tour reset
-- (user_onboarding.feature_tasks_reset_at) is replaced by the next one.
-- See api/crates/accounts/src/routes/onboarding/store.rs.
CREATE TABLE user_feature_task_sightings (
    user_id       INT          NOT NULL,
    company_id    INT          NOT NULL,
    task_id       VARCHAR(64)  NOT NULL,
    first_seen_at DATETIME(3)  NOT NULL,
    PRIMARY KEY (user_id, task_id),
    KEY idx_user_feature_task_sightings_company (company_id, task_id),
    CONSTRAINT fk_user_feature_task_sightings_user
        FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    CONSTRAINT fk_user_feature_task_sightings_company
        FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
