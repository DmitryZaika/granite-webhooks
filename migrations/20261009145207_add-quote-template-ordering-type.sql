-- Quote email templates get the same per-user drag-and-drop order as email
-- and SMS templates. Existing 'sms' and 'email' rows are unchanged.
ALTER TABLE user_template_order
  MODIFY COLUMN ordering_type ENUM('sms', 'email', 'quote') NOT NULL;

ALTER TABLE user_template_order_mode
  MODIFY COLUMN ordering_type ENUM('sms', 'email', 'quote') NOT NULL;
