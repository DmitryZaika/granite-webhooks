CREATE TABLE IF NOT EXISTS cloudtalk_sms_thread_reads (
  user_id INT NOT NULL,
  company_id INT NOT NULL,
  customer_phone_digits VARCHAR(20) NOT NULL,
  last_read_at DATETIME NOT NULL,
  PRIMARY KEY (user_id, company_id, customer_phone_digits),
  KEY idx_company_user (company_id, user_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
