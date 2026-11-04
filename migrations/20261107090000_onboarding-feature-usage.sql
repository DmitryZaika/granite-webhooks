-- "Already used" detection for guided tours: a user who has used a feature
-- never gets its tour by itself (it stays a 0-point replay), and a tour row
-- created after an old use pays no points.
--
-- 1. user_feature_usage caches the positives of the usage checks (a check is
--    a question like "did this user ever email a quote?"; see
--    api/crates/accounts/src/routes/onboarding/usage.rs). Only positives are
--    stored: a negative is asked again. first_used_at is when the use
--    happened, detected_at when we found out; source says whether the check
--    read the app's own data or the app reported a client-side event.
CREATE TABLE user_feature_usage (
    user_id       INT          NOT NULL,
    company_id    INT          NOT NULL,
    check_id      VARCHAR(64)  NOT NULL,
    first_used_at DATETIME(3)  NOT NULL,
    source        ENUM('data', 'event') NOT NULL,
    detected_at   DATETIME(3)  NOT NULL,
    PRIMARY KEY (user_id, company_id, check_id),
    KEY idx_user_feature_usage_company (company_id, check_id),
    CONSTRAINT fk_user_feature_usage_user
        FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    CONSTRAINT fk_user_feature_usage_company
        FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- 2. A tour's row remembers how it started and whether the user had used the
--    feature already (such a tour pays 0 points).
ALTER TABLE user_feature_tasks
    ADD COLUMN usage_skipped_at DATETIME(3) NULL,
    ADD COLUMN start_trigger ENUM('auto', 'user', 'gesture') NULL;

-- 3. Indexes the usage checks need (each check filters by the acting user).
--    Added only where no index leads with the column:
--    quote_emails(sent_by): only quote_id and template_id are indexed.
ALTER TABLE quote_emails ADD INDEX idx_quote_emails_sent_by (sent_by);
--    delayed_emails / delayed_sms: only (status, send_at) is indexed.
ALTER TABLE delayed_emails ADD INDEX idx_delayed_emails_user (user_id, company_id);
ALTER TABLE delayed_sms ADD INDEX idx_delayed_sms_user (user_id, company_id);
--    Skipped, already indexed: sales_questionnaire_responses(answered_by) has
--    the foreign key fk_questionnaire_response_user, which MySQL backs with an
--    index on answered_by; events(created_user_id) (fk_events_created_user),
--    call_coaching_analyses(user_id) (idx_call_coaching_user),
--    user_template_usage(user_id) (primary key), email_templates and
--    cloudtalk_sms_templates (company_id, user_id, ...) and
--    signature_requests(company_id, ...) (idx_signature_requests_customer)
--    lead with a column the checks filter by.
