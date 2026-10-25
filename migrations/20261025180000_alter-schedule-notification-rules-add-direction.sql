-- A flow step goes out before the appointment starts, or after it ends.
ALTER TABLE schedule_notification_rules
    ADD COLUMN direction ENUM('before', 'after') NOT NULL DEFAULT 'before' AFTER offset_unit;
