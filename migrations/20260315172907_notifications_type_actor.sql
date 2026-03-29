ALTER TABLE notifications
  ADD COLUMN notification_type VARCHAR(50) NULL AFTER message,
  ADD COLUMN actor_name VARCHAR(255) NULL AFTER notification_type;