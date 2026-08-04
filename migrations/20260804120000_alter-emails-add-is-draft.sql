ALTER TABLE emails
    ADD COLUMN is_draft TINYINT(1) NOT NULL DEFAULT 0;

CREATE INDEX idx_emails_is_draft ON emails (is_draft, deleted_at, sent_at);
