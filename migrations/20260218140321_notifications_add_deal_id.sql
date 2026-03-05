ALTER TABLE notifications
  ADD COLUMN deal_id BIGINT UNSIGNED NULL AFTER customer_id,
  ADD CONSTRAINT fk_notifications_deal FOREIGN KEY (deal_id) REFERENCES deals(id) ON DELETE SET NULL;