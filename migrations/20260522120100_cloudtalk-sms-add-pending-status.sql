ALTER TABLE cloudtalk_sms
  MODIFY COLUMN status ENUM('received', 'sent', 'failed', 'pending') NOT NULL DEFAULT 'received';
